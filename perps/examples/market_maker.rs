//! Market maker: maintains one bid and one ask around the oracle price.
//!
//! Every iteration the MM:
//!   1. Reads state: oracle price, current orderbook, pending queue
//!   2. Reconciles local order state against on-chain reality
//!   3. Computes target bid = oracle − spread, target ask = oracle + spread
//!   4. Decides what action each side needs (cancel+replace, place, etc.)
//!      Both decisions are made before any IO so seqs are allocated upfront.
//!   5. Executes both sides concurrently via tokio::join! — bid and ask
//!      transactions are in-flight simultaneously, halving per-iteration
//!      latency compared to the old sequential approach.
//!
//! Order tracking is done locally. `order_seq` is a client-side counter that
//! we pass to PlaceOrder and reuse in Cancel to identify the exact order.
//! On startup the MM scans the live book and queue to recover any existing orders.
//!
//! Usage:
//!   cargo run --example market_maker -- \
//!     --state <STATE_PUBKEY>          (required)
//!     [--rpc <URL>]                   (default: devnet)
//!     [--keypair <PATH>]              (default: ~/.config/solana/id.json)
//!     [--spread <N>]                  price units each side (default: 5)
//!     [--size <N>]                    order size (default: 10)
//!     [--requote-threshold <N>]       oracle move triggering requote (default: 3)
//!     [--interval-ms <N>]             (default: 2000)
//!     [--deposit <N>]                 one-time deposit on startup (default: 0)
//!
//! Environment variable alternatives:
//!   PERPS_STATE_ACCOUNT, PERPS_RPC_URL, PERPS_KEYPAIR

use std::str::FromStr;
use std::time::Duration;

use perps::state::PerpsState;
use solana_commitment_config::CommitmentConfig;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_signer::Signer;
use solana_transaction::Transaction;
use sokoban::NodeAllocatorMap;

const PERPS_PROGRAM_ID: Pubkey =
    solana_pubkey::pubkey!("HBtR4MuDfC6unTEcC1buv5u6ubJ2yRxTpvFWtvAXKwQC");

struct Config {
    state: Pubkey,
    rpc_url: String,
    keypair_path: String,
    spread: u64,
    size: u64,
    requote_threshold: u64,
    interval_ms: u64,
    deposit: u64,
}

/// Tracks the state of one side (bid or ask) of our quote.
#[derive(Debug, Clone)]
#[allow(dead_code)]
enum OrderStatus {
    /// No order — need to place one.
    None,
    /// PlaceOrder is in the queue; hasn't been cranked yet.
    Pending { price: u64, order_seq: u64 },
    /// Order is live on the book.
    Live { price: u64, order_seq: u64 },
    /// Cancel has been queued; waiting for crank.
    Cancelling { price: u64, order_seq: u64 },
    /// Cancel of old order AND a replacement PlaceOrder are both in the queue.
    /// We skip a full crank cycle by sending both instructions together.
    CancellingAndReplacing { old_seq: u64, new_price: u64, new_seq: u64 },
}

struct MmState {
    bid: OrderStatus,
    ask: OrderStatus,
    /// Monotonically increasing counter used as the `order_seq` argument to
    /// PlaceOrder. This lets us cancel a specific order without ambiguity.
    next_order_seq: u64,
}

impl MmState {
    fn new() -> Self {
        Self {
            bid: OrderStatus::None,
            ask: OrderStatus::None,
            next_order_seq: 1000, // start high to avoid collisions with any existing orders
        }
    }

    fn alloc_seq(&mut self) -> u64 {
        let seq = self.next_order_seq;
        self.next_order_seq += 1;
        seq
    }
}

/// What a single side needs to do this iteration.
/// Computed before any IO so both seqs are allocated upfront and both sides
/// can be executed concurrently.
enum SideAction {
    /// No transaction needed this iteration.
    Nothing,
    /// Queue a new resting order.
    Place { price: u64, size: u64, seq: u64 },
    /// Cancel a stale live order and immediately queue its replacement in one tx.
    CancelAndPlace { old_price: u64, old_seq: u64, new_price: u64, size: u64, new_seq: u64 },
    /// A cancel was already queued in a prior iteration; queue only the replacement now.
    PlaceReplacement { old_seq: u64, new_price: u64, size: u64, new_seq: u64 },
}

#[tokio::main]
async fn main() {
    let cfg = parse_config();
    let keypair = load_keypair(&cfg.keypair_path);
    let client = RpcClient::new_with_commitment(cfg.rpc_url.clone(), CommitmentConfig::confirmed());
    let my_pubkey = keypair.pubkey();
    let my_bytes: [u8; 32] = my_pubkey.to_bytes();

    println!("=== ACE Perps Market Maker ===");
    println!("State account: {}", cfg.state);
    println!("MM pubkey    : {}", my_pubkey);
    println!("RPC endpoint : {}", cfg.rpc_url);
    println!("Spread       : ±{}", cfg.spread);
    println!("Size         : {}", cfg.size);
    println!("Requote at   : oracle move > {}", cfg.requote_threshold);
    println!("Interval     : {}ms", cfg.interval_ms);
    println!();

    // Optional one-time deposit on startup
    if cfg.deposit > 0 {
        println!("Depositing {} collateral...", cfg.deposit);
        match send_sync_ix(&client, &keypair, &cfg.state, 0, cfg.deposit).await {
            Ok(sig) => println!("Deposit confirmed: {sig}"),
            Err(e) => eprintln!("Deposit failed: {e}"),
        }
    }

    let mut mm = MmState::new();

    // Recover any existing orders from the live book on startup
    recover_state(&client, &cfg.state, &my_bytes, &mut mm).await;

    loop {
        match run_iteration(&client, &keypair, &cfg, &my_bytes, &mut mm).await {
            Ok(()) => {}
            Err(e) => eprintln!("[mm error] {e}"),
        }
        tokio::time::sleep(Duration::from_millis(cfg.interval_ms)).await;
    }
}

async fn run_iteration(
    client: &RpcClient,
    keypair: &Keypair,
    cfg: &Config,
    my_bytes: &[u8; 32],
    mm: &mut MmState,
) -> Result<(), Box<dyn std::error::Error>> {
    let account = client.get_account(&cfg.state).await?;
    if account.data.len() < std::mem::size_of::<PerpsState>() {
        return Err("state account too small".into());
    }
    let state: &PerpsState =
        bytemuck::try_from_bytes(&account.data).map_err(|e| format!("deserialize: {e}"))?;

    let oracle = state.oracle_price;
    let target_bid = oracle.saturating_sub(cfg.spread);
    let target_ask = oracle + cfg.spread;
    let my_margin = state.margins.get_balance(my_bytes);

    // ── Reconcile bid/ask status against on-chain state ───────────────────────
    mm.bid = reconcile_status(mm.bid.clone(), my_bytes, state, 0);
    mm.ask = reconcile_status(mm.ask.clone(), my_bytes, state, 1);

    println!(
        "[mm] oracle={oracle}  target bid={target_bid} ask={target_ask}  margin={my_margin}  bid={:?}  ask={:?}",
        mm.bid, mm.ask
    );

    // ── Decide both actions before any IO ─────────────────────────────────────
    // Seqs are allocated here so neither side is blocked waiting for the other.
    let bid_action = decide_action(mm, state, my_bytes, cfg, my_margin, target_bid, 0);
    let ask_action = decide_action(mm, state, my_bytes, cfg, my_margin, target_ask, 1);

    // ── Execute both sides concurrently ───────────────────────────────────────
    let (bid_res, ask_res) = tokio::join!(
        execute_side_action(client, keypair, &cfg.state, &bid_action, 0),
        execute_side_action(client, keypair, &cfg.state, &ask_action, 1),
    );

    // ── Apply results (no IO, always succeeds) ────────────────────────────────
    mm.bid = apply_side(mm.bid.clone(), bid_action, bid_res, 0);
    mm.ask = apply_side(mm.ask.clone(), ask_action, ask_res, 1);

    Ok(())
}

// ─── Decide / execute / apply ─────────────────────────────────────────────────

/// Pure function: inspect the current status for one side and return the action
/// needed. Allocates a new `order_seq` via `mm.alloc_seq()` only if an order
/// will actually be sent, so seq space isn't wasted on no-ops.
fn decide_action(
    mm: &mut MmState,
    state: &PerpsState,
    my_bytes: &[u8; 32],
    cfg: &Config,
    my_margin: u64,
    target_price: u64,
    side: u64,
) -> SideAction {
    let status = if side == 0 { mm.bid.clone() } else { mm.ask.clone() };

    match status {
        OrderStatus::Live { price, order_seq } => {
            let delta = price.abs_diff(target_price);
            if delta > cfg.requote_threshold {
                let new_seq = mm.alloc_seq();
                println!(
                    "[mm] {} stale (price={price} target={target_price} delta={delta}), \
                     cancel+replace immediately seq={new_seq}",
                    side_str(side)
                );
                SideAction::CancelAndPlace {
                    old_price: price,
                    old_seq: order_seq,
                    new_price: target_price,
                    size: cfg.size,
                    new_seq,
                }
            } else {
                SideAction::Nothing
            }
        }
        OrderStatus::Cancelling { order_seq: old_seq, .. } => {
            // Cancel queued from a prior session; opportunistically place the replacement.
            let margin_needed = target_price * cfg.size / 10;
            if my_margin >= margin_needed && !is_pending_in_queue(state, my_bytes, side) {
                let new_seq = mm.alloc_seq();
                println!(
                    "[mm] {} cancel already queued, placing replacement seq={new_seq}",
                    side_str(side)
                );
                SideAction::PlaceReplacement {
                    old_seq,
                    new_price: target_price,
                    size: cfg.size,
                    new_seq,
                }
            } else {
                SideAction::Nothing
            }
        }
        OrderStatus::CancellingAndReplacing { .. } | OrderStatus::Pending { .. } => {
            // Already in flight — nothing to do this iteration.
            SideAction::Nothing
        }
        OrderStatus::None => {
            let margin_needed = target_price * cfg.size / 10;
            if my_margin >= margin_needed {
                let seq = mm.alloc_seq();
                println!(
                    "[mm] placing {}: {} @ {target_price}  seq={seq}",
                    side_str(side),
                    cfg.size
                );
                SideAction::Place { price: target_price, size: cfg.size, seq }
            } else {
                println!(
                    "[mm] insufficient margin for {} ({my_margin} < {margin_needed})",
                    side_str(side)
                );
                SideAction::Nothing
            }
        }
    }
}

/// Async: send whatever transaction the action requires. Returns immediately
/// for `Nothing`. For all other variants the call blocks until the transaction
/// is confirmed, but since both sides are joined concurrently in `run_iteration`
/// neither side waits for the other.
async fn execute_side_action(
    client: &RpcClient,
    keypair: &Keypair,
    state: &Pubkey,
    action: &SideAction,
    side: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    match action {
        SideAction::Nothing => Ok(()),
        SideAction::Place { price, size, seq } => {
            send_place_order(client, keypair, state, *price, *size, side, *seq).await
        }
        SideAction::CancelAndPlace { old_price, old_seq, new_price, size, new_seq } => {
            send_cancel_and_place(
                client, keypair, state,
                *old_price, *old_seq,
                *new_price, *size, side, *new_seq,
            ).await
        }
        SideAction::PlaceReplacement { new_price, size, new_seq, .. } => {
            send_place_order(client, keypair, state, *new_price, *size, side, *new_seq).await
        }
    }
}

/// Pure: translate (old status, action that was attempted, send result) into
/// the new local status. On error we keep the old status so the next iteration
/// can retry; the other side's state is updated independently.
fn apply_side(
    status: OrderStatus,
    action: SideAction,
    result: Result<(), Box<dyn std::error::Error>>,
    side: u64,
) -> OrderStatus {
    if let Err(e) = result {
        eprintln!("[mm] {} tx failed: {e}", side_str(side));
        return status; // retry next iteration
    }
    match action {
        SideAction::Nothing => status,
        SideAction::Place { price, seq, .. } => {
            OrderStatus::Pending { price, order_seq: seq }
        }
        SideAction::CancelAndPlace { old_seq, new_price, new_seq, .. } => {
            OrderStatus::CancellingAndReplacing { old_seq, new_price, new_seq }
        }
        SideAction::PlaceReplacement { old_seq, new_price, new_seq, .. } => {
            OrderStatus::CancellingAndReplacing { old_seq, new_price, new_seq }
        }
    }
}

// ─── Reconcile ────────────────────────────────────────────────────────────────

/// Reconcile our local order status against on-chain state.
fn reconcile_status(
    current: OrderStatus,
    my_bytes: &[u8; 32],
    state: &PerpsState,
    side: u64,
) -> OrderStatus {
    match current {
        OrderStatus::Pending { price, order_seq } => {
            if is_on_book(state, my_bytes, order_seq, side) {
                println!("[mm] {} order {order_seq} is now live", side_str(side));
                OrderStatus::Live { price, order_seq }
            } else if !is_pending_in_queue(state, my_bytes, side) {
                println!("[mm] {} PlaceOrder {order_seq} dropped", side_str(side));
                OrderStatus::None
            } else {
                current
            }
        }
        OrderStatus::Live { price: _, order_seq } => {
            if is_on_book(state, my_bytes, order_seq, side) {
                current
            } else {
                println!("[mm] {} order {order_seq} gone (filled or cancelled)", side_str(side));
                OrderStatus::None
            }
        }
        OrderStatus::Cancelling { .. } => {
            if !is_cancel_pending_in_queue(state, my_bytes, side) {
                println!("[mm] {} cancel confirmed", side_str(side));
                OrderStatus::None
            } else {
                current
            }
        }
        OrderStatus::CancellingAndReplacing { new_price, new_seq, .. } => {
            if is_on_book(state, my_bytes, new_seq, side) {
                println!("[mm] {} replacement {new_seq} is now live", side_str(side));
                OrderStatus::Live { price: new_price, order_seq: new_seq }
            } else if is_place_seq_pending_in_queue(state, my_bytes, new_seq, side) {
                current
            } else {
                println!("[mm] {} replacement {new_seq} dropped", side_str(side));
                OrderStatus::None
            }
        }
        OrderStatus::None => OrderStatus::None,
    }
}

// ─── Queue / book inspection helpers ─────────────────────────────────────────

fn is_on_book(state: &PerpsState, my_bytes: &[u8; 32], order_seq: u64, side: u64) -> bool {
    if side == 0 {
        state.orderbook.bids.iter()
            .any(|(key, order)| &order.owner == my_bytes && key.seq == order_seq)
    } else {
        state.orderbook.asks.iter()
            .any(|(key, order)| &order.owner == my_bytes && key.seq == order_seq)
    }
}

/// Returns true if any PlaceOrder from us (for the given side) is in the queue.
fn is_pending_in_queue(state: &PerpsState, my_bytes: &[u8; 32], side: u64) -> bool {
    state.async_queue.iter()
        .any(|(_, q)| q.ix_type == 2 && &q.user == my_bytes && q.side == side)
}

/// Returns true if a specific PlaceOrder (by order_seq) from us is in the queue.
fn is_place_seq_pending_in_queue(
    state: &PerpsState,
    my_bytes: &[u8; 32],
    order_seq: u64,
    side: u64,
) -> bool {
    state.async_queue.iter()
        .any(|(_, q)| q.ix_type == 2 && &q.user == my_bytes && q.side == side && q.order_seq == order_seq)
}

/// Returns true if a Cancel from us (for the given side) is in the queue.
fn is_cancel_pending_in_queue(state: &PerpsState, my_bytes: &[u8; 32], side: u64) -> bool {
    state.async_queue.iter()
        .any(|(_, q)| q.ix_type == 1 && &q.user == my_bytes && q.side == side)
}

// ─── Startup recovery ─────────────────────────────────────────────────────────

async fn recover_state(
    client: &RpcClient,
    state_pubkey: &Pubkey,
    my_bytes: &[u8; 32],
    mm: &mut MmState,
) {
    let Ok(account) = client.get_account(state_pubkey).await else { return };
    if account.data.len() < std::mem::size_of::<PerpsState>() {
        return;
    }
    let Ok(state) = bytemuck::try_from_bytes::<PerpsState>(&account.data) else { return };

    // Scan live bids
    for (key, order) in state.orderbook.bids.iter() {
        if &order.owner == my_bytes {
            let real_price = u64::MAX - key.price_key;
            println!("[mm] recovered live bid: price={real_price} seq={}", key.seq);
            mm.bid = OrderStatus::Live { price: real_price, order_seq: key.seq };
            mm.next_order_seq = mm.next_order_seq.max(key.seq + 1);
        }
    }
    // Scan live asks
    for (key, order) in state.orderbook.asks.iter() {
        if &order.owner == my_bytes {
            println!("[mm] recovered live ask: price={} seq={}", key.price_key, key.seq);
            mm.ask = OrderStatus::Live { price: key.price_key, order_seq: key.seq };
            mm.next_order_seq = mm.next_order_seq.max(key.seq + 1);
        }
    }

    // Scan pending queue: collect cancel and place entries per side, then merge.
    let mut bid_cancel: Option<(u64, u64)> = None; // (price, old_seq)
    let mut bid_place: Option<(u64, u64)> = None;  // (price, new_seq)
    let mut ask_cancel: Option<(u64, u64)> = None;
    let mut ask_place: Option<(u64, u64)> = None;

    for (_, q) in state.async_queue.iter() {
        if &q.user != my_bytes {
            continue;
        }
        match (q.ix_type, q.side) {
            (1, 0) => { bid_cancel = Some((q.price, q.order_seq)); }
            (2, 0) => { bid_place  = Some((q.price, q.order_seq)); }
            (1, 1) => { ask_cancel = Some((q.price, q.order_seq)); }
            (2, 1) => { ask_place  = Some((q.price, q.order_seq)); }
            _ => {}
        }
        mm.next_order_seq = mm.next_order_seq.max(q.order_seq + 1);
    }

    match (bid_cancel, bid_place) {
        (Some((_, old_seq)), Some((new_price, new_seq))) => {
            println!("[mm] recovered bid CancellingAndReplacing: old_seq={old_seq} new_price={new_price} new_seq={new_seq}");
            mm.bid = OrderStatus::CancellingAndReplacing { old_seq, new_price, new_seq };
        }
        (Some((price, seq)), None) => {
            println!("[mm] recovered queued bid Cancel: seq={seq}");
            mm.bid = OrderStatus::Cancelling { price, order_seq: seq };
        }
        (None, Some((price, seq))) => {
            println!("[mm] recovered queued bid PlaceOrder: price={price} seq={seq}");
            mm.bid = OrderStatus::Pending { price, order_seq: seq };
        }
        (None, None) => {}
    }

    match (ask_cancel, ask_place) {
        (Some((_, old_seq)), Some((new_price, new_seq))) => {
            println!("[mm] recovered ask CancellingAndReplacing: old_seq={old_seq} new_price={new_price} new_seq={new_seq}");
            mm.ask = OrderStatus::CancellingAndReplacing { old_seq, new_price, new_seq };
        }
        (Some((price, seq)), None) => {
            println!("[mm] recovered queued ask Cancel: seq={seq}");
            mm.ask = OrderStatus::Cancelling { price, order_seq: seq };
        }
        (None, Some((price, seq))) => {
            println!("[mm] recovered queued ask PlaceOrder: price={price} seq={seq}");
            mm.ask = OrderStatus::Pending { price, order_seq: seq };
        }
        (None, None) => {}
    }
}

// ─── Instruction builders ─────────────────────────────────────────────────────

async fn send_sync_ix(
    client: &RpcClient,
    keypair: &Keypair,
    state: &Pubkey,
    ix_type: u64,
    param: u64,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut data = vec![0u8]; // route 0 = sync
    data.extend_from_slice(&ix_type.to_le_bytes());
    data.extend_from_slice(&param.to_le_bytes());

    let ix = Instruction {
        program_id: PERPS_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*state, false),
            AccountMeta::new_readonly(keypair.pubkey(), true),
        ],
        data,
    };
    send_tx(client, keypair, &[ix]).await
}

async fn send_place_order(
    client: &RpcClient,
    keypair: &Keypair,
    state: &Pubkey,
    price: u64,
    size: u64,
    side: u64,
    order_seq: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut data = vec![1u8]; // route 1 = async
    data.extend_from_slice(&2u64.to_le_bytes()); // ix_type = PlaceOrder
    data.extend_from_slice(&price.to_le_bytes());
    data.extend_from_slice(&size.to_le_bytes());
    data.extend_from_slice(&side.to_le_bytes());
    data.extend_from_slice(&order_seq.to_le_bytes());

    let ix = Instruction {
        program_id: PERPS_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*state, false),
            AccountMeta::new_readonly(keypair.pubkey(), true),
        ],
        data,
    };
    send_tx(client, keypair, &[ix]).await?;
    Ok(())
}

/// Queues a Cancel and a replacement PlaceOrder in a single transaction so we
/// don't waste a crank cycle waiting for the cancel to land before placing the
/// new order. Both instructions share the same signer and state account.
async fn send_cancel_and_place(
    client: &RpcClient,
    keypair: &Keypair,
    state: &Pubkey,
    old_price: u64,
    old_order_seq: u64,
    new_price: u64,
    new_size: u64,
    side: u64,
    new_order_seq: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let accounts = vec![
        AccountMeta::new(*state, false),
        AccountMeta::new_readonly(keypair.pubkey(), true),
    ];

    // Cancel instruction
    let mut cancel_data = vec![1u8]; // route 1 = async
    cancel_data.extend_from_slice(&1u64.to_le_bytes()); // ix_type = Cancel
    cancel_data.extend_from_slice(&old_price.to_le_bytes());
    cancel_data.extend_from_slice(&0u64.to_le_bytes()); // size unused for cancel
    cancel_data.extend_from_slice(&side.to_le_bytes());
    cancel_data.extend_from_slice(&old_order_seq.to_le_bytes());

    // PlaceOrder instruction
    let mut place_data = vec![1u8]; // route 1 = async
    place_data.extend_from_slice(&2u64.to_le_bytes()); // ix_type = PlaceOrder
    place_data.extend_from_slice(&new_price.to_le_bytes());
    place_data.extend_from_slice(&new_size.to_le_bytes());
    place_data.extend_from_slice(&side.to_le_bytes());
    place_data.extend_from_slice(&new_order_seq.to_le_bytes());

    let ixs = [
        Instruction { program_id: PERPS_PROGRAM_ID, accounts: accounts.clone(), data: cancel_data },
        Instruction { program_id: PERPS_PROGRAM_ID, accounts,                  data: place_data  },
    ];
    send_tx(client, keypair, &ixs).await?;
    Ok(())
}

async fn send_tx(
    client: &RpcClient,
    keypair: &Keypair,
    ixs: &[Instruction],
) -> Result<String, Box<dyn std::error::Error>> {
    let blockhash = client.get_latest_blockhash().await?;
    let tx = Transaction::new_signed_with_payer(
        ixs,
        Some(&keypair.pubkey()),
        &[keypair],
        blockhash,
    );
    let sig = client.send_and_confirm_transaction(&tx).await?;
    Ok(sig.to_string())
}

fn side_str(side: u64) -> &'static str {
    if side == 0 { "bid" } else { "ask" }
}

// ─── Config / keypair helpers ─────────────────────────────────────────────────

fn parse_config() -> Config {
    let args: Vec<String> = std::env::args().collect();

    let state_str = arg_val(&args, "--state")
        .or_else(|| std::env::var("PERPS_STATE_ACCOUNT").ok())
        .expect("Required: --state <PUBKEY>  or  PERPS_STATE_ACCOUNT=<PUBKEY>");

    Config {
        state: Pubkey::from_str(&state_str).expect("invalid state pubkey"),
        rpc_url: arg_val(&args, "--rpc")
            .or_else(|| std::env::var("PERPS_RPC_URL").ok())
            .unwrap_or_else(|| "https://api.devnet.solana.com".to_string()),
        keypair_path: arg_val(&args, "--keypair")
            .or_else(|| std::env::var("PERPS_KEYPAIR").ok())
            .unwrap_or_else(default_keypair_path),
        spread: arg_val(&args, "--spread")
            .and_then(|s| s.parse().ok())
            .unwrap_or(5),
        size: arg_val(&args, "--size")
            .and_then(|s| s.parse().ok())
            .unwrap_or(10),
        requote_threshold: arg_val(&args, "--requote-threshold")
            .and_then(|s| s.parse().ok())
            .unwrap_or(3),
        interval_ms: arg_val(&args, "--interval-ms")
            .or_else(|| std::env::var("PERPS_INTERVAL_MS").ok())
            .and_then(|s| s.parse().ok())
            .unwrap_or(2000),
        deposit: arg_val(&args, "--deposit")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
    }
}

fn arg_val(args: &[String], flag: &str) -> Option<String> {
    let pos = args.iter().position(|a| a == flag)?;
    args.get(pos + 1).cloned()
}

fn default_keypair_path() -> String {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".to_string());
    format!("{home}/.config/solana/id.json")
}

fn load_keypair(path: &str) -> Keypair {
    solana_keypair::read_keypair_file(path)
        .unwrap_or_else(|e| panic!("cannot load keypair from '{path}': {e}"))
}
