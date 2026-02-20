//! Crank: connects to devnet and processes queued perps instructions whose slot
//! threshold has passed (queued_slot + 1 <= current_slot).
//!
//! Prerequisites:
//!   - Program deployed at PERPS_PROGRAM_ID
//!   - State account created and initialized (send any instruction once)
//!   - Payer keypair funded with SOL for transaction fees
//!
//! Usage:
//!   cargo run --example crank -- \
//!     --state <STATE_PUBKEY>      (required)
//!     [--rpc   <URL>]             (default: devnet)
//!     [--keypair <PATH>]          (default: ~/.config/solana/id.json)
//!     [--interval-ms <N>]         (default: 500)
//!
//! Environment variable alternatives:
//!   PERPS_STATE_ACCOUNT, PERPS_RPC_URL, PERPS_KEYPAIR, PERPS_INTERVAL_MS

use std::str::FromStr;
use std::thread;
use std::time::Duration;

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
    interval_ms: u64,
}

fn main() {
    let cfg = parse_config();
    let keypair = load_keypair(&cfg.keypair_path);
    let client = RpcClient::new_with_commitment(cfg.rpc_url.clone(), CommitmentConfig::confirmed());

    println!("=== ACE Perps Crank ===");
    println!("State account : {}", cfg.state);
    println!("Cranker pubkey: {}", keypair.pubkey());
    println!("RPC endpoint  : {}", cfg.rpc_url);
    println!("Poll interval : {}ms", cfg.interval_ms);
    println!();

    let mut iters: u64 = 0;
    loop {
        match try_crank(&client, &keypair, &cfg.state) {
            Ok(cranked) => {
                if !cranked {
                    iters += 1;
                    // Print a heartbeat dot every ~5 s to show we're alive
                    if iters % (5000 / cfg.interval_ms.max(1)) == 0 {
                        print!(".");
                        use std::io::Write;
                        std::io::stdout().flush().ok();
                    }
                } else {
                    iters = 0;
                }
            }
            Err(e) => eprintln!("\n[crank error] {e}"),
        }
        thread::sleep(Duration::from_millis(cfg.interval_ms));
    }
}

fn try_crank(
    client: &RpcClient,
    keypair: &Keypair,
    state_pubkey: &Pubkey,
) -> Result<bool, Box<dyn std::error::Error>> {
    let account = client.get_account(state_pubkey)?;
    if account.data.len() < std::mem::size_of::<PerpsState>() {
        return Err(format!(
            "state account too small ({} < {})",
            account.data.len(),
            std::mem::size_of::<PerpsState>()
        )
        .into());
    }

    let state: &PerpsState =
        bytemuck::try_from_bytes(&account.data).map_err(|e| format!("deserialize: {e}"))?;

    // peek_async walks the RBTree to the leftmost (minimum-key) node, which
    // is the next item that would be processed by the crank.
    let Some((_, next_node)) = state.peek_async() else {
        return Ok(false); // queue empty
    };

    let current_slot = client.get_slot()?;

    // The crank is only allowed to process instructions from previous slots
    // (queued_slot + 1 <= current_slot), ensuring at least one full slot of
    // priority ordering has been established.
    if next_node.key.slot + 1 > current_slot {
        return Ok(false); // too early
    }

    let queue_depth = state.async_queue.len();
    let oldest_slot = next_node.key.slot;
    println!(
        "\n[slot={current_slot}] queue depth={queue_depth}, oldest item from slot={oldest_slot}"
    );

    // Print what's about to be processed
    print_queue_summary(state, current_slot);

    // Build and send the crank transaction.
    // Route byte 2 tells the program to drain all processable queue items.
    let ix = Instruction {
        program_id: PERPS_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*state_pubkey, false),
            AccountMeta::new_readonly(keypair.pubkey(), true),
        ],
        data: vec![2u8],
    };

    let blockhash = client.get_latest_blockhash()?;
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&keypair.pubkey()),
        &[keypair],
        blockhash,
    );

    let sig = client.send_and_confirm_transaction(&tx)?;
    println!("[crank] confirmed: {sig}");
    Ok(true)
}

fn print_queue_summary(state: &PerpsState, current_slot: u64) {
    let processable: Vec<_> = state
        .async_queue
        .iter()
        .filter(|(k, _)| k.slot + 1 <= current_slot)
        .collect();

    if processable.is_empty() {
        return;
    }

    println!("  Items ready to process:");
    // Sort by (slot, priority, seq) for display
    let mut sorted = processable;
    sorted.sort_by_key(|(k, _)| (k.slot, k.priority, k.seq));

    for (key, val) in &sorted {
        let ix_name = ix_type_name(val.ix_type);
        let owner = short_key(&val.user);
        println!(
            "    slot={} pri={} seq={} → {ix_name} by {owner}  price={} size={} side={}",
            key.slot, key.priority, key.seq, val.price, val.size, side_name(val.side)
        );
    }
}

fn ix_type_name(t: u64) -> &'static str {
    match t {
        0 => "Liquidate",
        1 => "Cancel",
        2 => "PlaceOrder",
        3 => "Take",
        _ => "Unknown",
    }
}

fn side_name(side: u64) -> &'static str {
    if side == 0 { "bid/buy" } else { "ask/sell" }
}

fn short_key(bytes: &[u8; 32]) -> String {
    let pk = Pubkey::new_from_array(*bytes);
    pk.to_string()[..8].to_string()
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
        interval_ms: arg_val(&args, "--interval-ms")
            .or_else(|| std::env::var("PERPS_INTERVAL_MS").ok())
            .and_then(|s| s.parse().ok())
            .unwrap_or(500),
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
