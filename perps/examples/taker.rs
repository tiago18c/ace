//! Taker: submits a single take order to devnet.
//!
//! Shows the current market state, then prompts for confirmation before
//! submitting. Can be used non-interactively by supplying all args on the
//! command line.
//!
//! A Take instruction queues a market-crossing order against the best available
//! price. Because Takes have the lowest priority (3) in the async queue, any
//! Cancels or Liquidations queued in the same slot execute first — the ACE
//! cancel-protection guarantee.
//!
//! Usage (interactive — prompts for missing fields):
//!   cargo run --example taker -- \
//!     --state <STATE_PUBKEY>
//!
//! Usage (non-interactive — all fields supplied):
//!   cargo run --example taker -- \
//!     --state <STATE_PUBKEY>  \
//!     --side buy              \   (buy | sell)
//!     --size 5                \
//!     --limit-price 155       \
//!     [--rpc <URL>]           \
//!     [--keypair <PATH>]      \
//!     [--deposit <N>]             one-time deposit before ordering
//!
//! Environment variable alternatives:
//!   PERPS_STATE_ACCOUNT, PERPS_RPC_URL, PERPS_KEYPAIR

use std::io::{self, BufRead, Write};
use std::str::FromStr;

use perps::state::PerpsState;
use solana_commitment_config::CommitmentConfig;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_rpc_client::rpc_client::RpcClient;
use solana_signer::Signer;
use solana_transaction::Transaction;
use sokoban::NodeAllocatorMap;

const PERPS_PROGRAM_ID: Pubkey =
    solana_pubkey::pubkey!("HBtR4MuDfC6unTEcC1buv5u6ubJ2yRxTpvFWtvAXKwQC");

struct Config {
    state: Pubkey,
    rpc_url: String,
    keypair_path: String,
    side: Option<u64>,        // 0 = buy, 1 = sell
    size: Option<u64>,
    limit_price: Option<u64>,
    deposit: u64,
}

fn main() {
    let cfg = parse_config();
    let keypair = load_keypair(&cfg.keypair_path);
    let client = RpcClient::new_with_commitment(cfg.rpc_url.clone(), CommitmentConfig::confirmed());
    let my_bytes: [u8; 32] = keypair.pubkey().to_bytes();

    println!("=== ACE Perps Taker ===");
    println!("State account: {}", cfg.state);
    println!("Taker pubkey : {}", keypair.pubkey());
    println!("RPC endpoint : {}", cfg.rpc_url);
    println!();

    // Optional one-time deposit
    if cfg.deposit > 0 {
        println!("Depositing {} collateral...", cfg.deposit);
        match send_sync_ix(&client, &keypair, &cfg.state, 0, cfg.deposit) {
            Ok(sig) => println!("Deposit confirmed: {sig}\n"),
            Err(e) => {
                eprintln!("Deposit failed: {e}");
                return;
            }
        }
    }

    // Fetch and display current market state
    let state_snap = match fetch_state(&client, &cfg.state) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Cannot read state: {e}");
            return;
        }
    };

    let my_margin = state_snap.margins.get_balance(&my_bytes);
    println!("Oracle price   : {}", state_snap.oracle_price);
    println!("Your margin    : {my_margin}");
    println!();
    print_book(&state_snap);
    println!();

    // Gather order parameters (from args or prompt)
    let side = cfg.side.unwrap_or_else(|| prompt_side());
    let size = cfg.size.unwrap_or_else(|| prompt_u64("Enter size: "));
    let limit_price = cfg.limit_price.unwrap_or_else(|| prompt_limit_price(side, &state_snap));

    // Validate: check best available price
    let (best_price, book_side_label) = if side == 0 {
        // Buying — crosses against asks
        match state_snap.orderbook.best_ask() {
            Some((_, o)) => (Some(o.price), "best ask"),
            None => (None, "best ask"),
        }
    } else {
        // Selling — crosses against bids
        match state_snap.orderbook.best_bid() {
            Some((_, o)) => (Some(o.price), "best bid"),
            None => (None, "best bid"),
        }
    };

    println!();
    println!("Order summary:");
    println!("  Side        : {}", if side == 0 { "BUY" } else { "SELL" });
    println!("  Size        : {size}");
    println!("  Limit price : {limit_price}  (your worst acceptable price)");
    println!("  {book_side_label} : {}", best_price.map(|p| p.to_string()).unwrap_or_else(|| "none".to_string()));

    let margin_needed = limit_price * size / 10;
    println!("  Margin req  : {margin_needed}  (10% of notional)");
    if my_margin < margin_needed {
        println!(
            "\nWARNING: your margin ({my_margin}) is below the required {margin_needed}. \
             The order will be queued but may fail when cranked."
        );
    }

    if best_price.is_none() {
        println!(
            "\nWARNING: the {} is empty. Your take will be queued but will fill 0 size \
             when cranked (no counterpart).",
            book_side_label
        );
    }

    // Confirm
    print!("\nSubmit take order? [y/N] ");
    io::stdout().flush().ok();
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line).ok();
    if !line.trim().eq_ignore_ascii_case("y") {
        println!("Cancelled.");
        return;
    }

    // Queue the Take instruction
    println!("\nQueuing Take...");
    match send_take(&client, &keypair, &cfg.state, limit_price, size, side) {
        Ok(sig) => {
            println!("Queued! Signature: {sig}");
            println!();
            println!("Note: the take will execute when a crank processes it in the next slot.");
            println!("      Run the crank example, or wait for an existing crank to pick it up.");
            println!("      Cancels in the same slot execute FIRST (ACE priority guarantee).");
        }
        Err(e) => eprintln!("Failed to queue take: {e}"),
    }
}

// ─── State helpers ────────────────────────────────────────────────────────────

/// Fetch and deserialize the state account. Returns an owned copy.
fn fetch_state(
    client: &RpcClient,
    state_pubkey: &Pubkey,
) -> Result<Box<PerpsState>, Box<dyn std::error::Error>> {
    let account = client.get_account(state_pubkey)?;
    if account.data.len() < std::mem::size_of::<PerpsState>() {
        return Err("state account too small".into());
    }
    let state: &PerpsState =
        bytemuck::try_from_bytes(&account.data).map_err(|e| format!("deserialize: {e}"))?;
    // Copy into a Box to own it (account.data is dropped)
    let layout = std::alloc::Layout::new::<PerpsState>();
    let ptr = unsafe { std::alloc::alloc(layout) as *mut PerpsState };
    unsafe { std::ptr::write(ptr, *state) };
    Ok(unsafe { Box::from_raw(ptr) })
}

fn print_book(state: &PerpsState) {
    let mut asks: Vec<(u64, u64)> = state
        .orderbook
        .asks
        .iter()
        .map(|(k, o)| (k.price_key, o.size))
        .collect();
    asks.sort_by(|a, b| a.0.cmp(&b.0));

    let mut bids: Vec<(u64, u64)> = state
        .orderbook
        .bids
        .iter()
        .map(|(k, o)| (u64::MAX - k.price_key, o.size))
        .collect();
    bids.sort_by(|a, b| b.0.cmp(&a.0));

    // Count queued PlaceOrders and Takes for context
    let queued_place_bids = state
        .async_queue
        .iter()
        .filter(|(_, q)| q.ix_type == 2 && q.side == 0)
        .count();
    let queued_place_asks = state
        .async_queue
        .iter()
        .filter(|(_, q)| q.ix_type == 2 && q.side == 1)
        .count();
    let queued_takes = state
        .async_queue
        .iter()
        .filter(|(_, q)| q.ix_type == 3)
        .count();

    println!(
        "Orderbook  ({} bids, {} asks)  +{} queued placements, {} pending takes",
        bids.len(),
        asks.len(),
        queued_place_bids + queued_place_asks,
        queued_takes
    );
    println!("  {:>10}  {:>8}  Side", "Price", "Size");
    println!("  {}", "-".repeat(28));

    for (price, size) in asks.iter().rev().take(8) {
        println!("  {:>10}  {:>8}  ASK", price, size);
    }

    let best_bid = bids.first().map(|(p, _)| *p);
    let best_ask = asks.first().map(|(p, _)| *p);
    match (best_bid, best_ask) {
        (Some(b), Some(a)) if a >= b => {
            println!("  ─── spread: {} ──────────────────", a - b)
        }
        _ => println!("  ─── (empty book) ───────────────────"),
    }

    for (price, size) in bids.iter().take(8) {
        println!("  {:>10}  {:>8}  BID", price, size);
    }
}

// ─── Interactive prompts ──────────────────────────────────────────────────────

fn prompt_side() -> u64 {
    loop {
        print!("Side (buy/sell): ");
        io::stdout().flush().ok();
        let mut line = String::new();
        io::stdin().lock().read_line(&mut line).ok();
        match line.trim().to_lowercase().as_str() {
            "buy" | "b" | "0" => return 0,
            "sell" | "s" | "1" => return 1,
            _ => println!("Please enter 'buy' or 'sell'"),
        }
    }
}

fn prompt_u64(prompt: &str) -> u64 {
    loop {
        print!("{prompt}");
        io::stdout().flush().ok();
        let mut line = String::new();
        io::stdin().lock().read_line(&mut line).ok();
        match line.trim().parse::<u64>() {
            Ok(v) if v > 0 => return v,
            _ => println!("Please enter a positive integer"),
        }
    }
}

fn prompt_limit_price(side: u64, state: &PerpsState) -> u64 {
    let suggestion = if side == 0 {
        // Buying: suggest best ask + some tolerance
        state
            .orderbook
            .best_ask()
            .map(|(_, o)| o.price + 2)
            .unwrap_or(state.oracle_price + 5)
    } else {
        // Selling: suggest best bid - some tolerance
        state
            .orderbook
            .best_bid()
            .map(|(_, o)| o.price.saturating_sub(2))
            .unwrap_or(state.oracle_price.saturating_sub(5))
    };
    println!("Suggested limit price: {suggestion}");
    prompt_u64("Limit price: ")
}

// ─── Instruction builders ─────────────────────────────────────────────────────

fn send_sync_ix(
    client: &RpcClient,
    keypair: &Keypair,
    state: &Pubkey,
    ix_type: u64,
    param: u64,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut data = vec![0u8];
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
    send_tx(client, keypair, &[ix])
}

fn send_take(
    client: &RpcClient,
    keypair: &Keypair,
    state: &Pubkey,
    limit_price: u64,
    size: u64,
    side: u64,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut data = vec![1u8]; // route 1 = async
    let ix_type: u64 = 3; // Take
    data.extend_from_slice(&ix_type.to_le_bytes());
    data.extend_from_slice(&limit_price.to_le_bytes());
    data.extend_from_slice(&size.to_le_bytes());
    data.extend_from_slice(&side.to_le_bytes());
    let order_seq: u64 = 0; // unused for Take
    data.extend_from_slice(&order_seq.to_le_bytes());

    let ix = Instruction {
        program_id: PERPS_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*state, false),
            AccountMeta::new_readonly(keypair.pubkey(), true),
        ],
        data,
    };
    send_tx(client, keypair, &[ix])
}

fn send_tx(
    client: &RpcClient,
    keypair: &Keypair,
    ixs: &[Instruction],
) -> Result<String, Box<dyn std::error::Error>> {
    let blockhash = client.get_latest_blockhash()?;
    let tx = Transaction::new_signed_with_payer(
        ixs,
        Some(&keypair.pubkey()),
        &[keypair],
        blockhash,
    );
    let sig = client.send_and_confirm_transaction(&tx)?;
    Ok(sig.to_string())
}

// ─── Config / keypair helpers ─────────────────────────────────────────────────

fn parse_config() -> Config {
    let args: Vec<String> = std::env::args().collect();

    let state_str = arg_val(&args, "--state")
        .or_else(|| std::env::var("PERPS_STATE_ACCOUNT").ok())
        .expect("Required: --state <PUBKEY>  or  PERPS_STATE_ACCOUNT=<PUBKEY>");

    let side = arg_val(&args, "--side").map(|s| match s.to_lowercase().as_str() {
        "buy" | "b" | "0" => 0u64,
        "sell" | "s" | "1" => 1u64,
        other => panic!("unknown side '{other}': use 'buy' or 'sell'"),
    });

    Config {
        state: Pubkey::from_str(&state_str).expect("invalid state pubkey"),
        rpc_url: arg_val(&args, "--rpc")
            .or_else(|| std::env::var("PERPS_RPC_URL").ok())
            .unwrap_or_else(|| "https://api.devnet.solana.com".to_string()),
        keypair_path: arg_val(&args, "--keypair")
            .or_else(|| std::env::var("PERPS_KEYPAIR").ok())
            .unwrap_or_else(default_keypair_path),
        side,
        size: arg_val(&args, "--size").and_then(|s| s.parse().ok()),
        limit_price: arg_val(&args, "--limit-price").and_then(|s| s.parse().ok()),
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
