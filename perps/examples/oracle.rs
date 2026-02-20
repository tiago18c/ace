//! Oracle price updater: sends UpdateOracle (sync ix type 2) to devnet.
//!
//! Modes:
//!   --price <N>          Set a specific price once and exit.
//!   --simulate           Simulate a price walk: random ±drift every interval.
//!                        Useful for testing the market maker's requote logic.
//!   (neither)            Interactive: prompts for a new price each time.
//!
//! Usage:
//!   cargo run --example oracle -- \
//!     --state <STATE_PUBKEY>              (required)
//!     [--price <N>]                       set price once and exit
//!     [--simulate]                        random walk simulation
//!     [--sim-start <N>]                   starting price for simulation (default: current)
//!     [--sim-step <N>]                    max price move per tick (default: 3)
//!     [--rpc <URL>]                       (default: devnet)
//!     [--keypair <PATH>]                  oracle authority keypair
//!     [--interval-ms <N>]                 simulation tick rate (default: 2000)
//!
//! Environment variable alternatives:
//!   PERPS_STATE_ACCOUNT, PERPS_RPC_URL, PERPS_KEYPAIR

use std::io::{self, BufRead, Write};
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

const PERPS_PROGRAM_ID: Pubkey =
    solana_pubkey::pubkey!("HBtR4MuDfC6unTEcC1buv5u6ubJ2yRxTpvFWtvAXKwQC");

#[derive(Debug)]
enum Mode {
    Once(u64),
    Simulate { start: Option<u64>, step: u64, interval_ms: u64 },
    Interactive,
}

struct Config {
    state: Pubkey,
    rpc_url: String,
    keypair_path: String,
    mode: Mode,
}

fn main() {
    let cfg = parse_config();
    let keypair = load_keypair(&cfg.keypair_path);
    let client = RpcClient::new_with_commitment(cfg.rpc_url.clone(), CommitmentConfig::confirmed());

    println!("=== ACE Perps Oracle ===");
    println!("State account   : {}", cfg.state);
    println!("Oracle authority: {}", keypair.pubkey());
    println!("RPC endpoint    : {}", cfg.rpc_url);
    println!("Mode            : {:?}", cfg.mode);
    println!();

    // Always show current state before doing anything
    match fetch_oracle_price(&client, &cfg.state) {
        Ok(price) => println!("Current oracle price: {price}"),
        Err(e) => eprintln!("Warning: could not read current price: {e}"),
    }
    println!();

    match cfg.mode {
        Mode::Once(price) => {
            set_price(&client, &keypair, &cfg.state, price);
        }
        Mode::Simulate { start, step, interval_ms } => {
            run_simulation(&client, &keypair, &cfg.state, start, step, interval_ms);
        }
        Mode::Interactive => {
            run_interactive(&client, &keypair, &cfg.state);
        }
    }
}

// ─── Modes ───────────────────────────────────────────────────────────────────

fn set_price(client: &RpcClient, keypair: &Keypair, state: &Pubkey, price: u64) {
    print!("Setting oracle price to {price}... ");
    io::stdout().flush().ok();
    match send_update_oracle(client, keypair, state, price) {
        Ok(sig) => {
            println!("ok");
            println!("Signature: {sig}");
            // Confirm by re-reading
            match fetch_oracle_price(client, state) {
                Ok(p) => println!("Confirmed on-chain price: {p}"),
                Err(e) => eprintln!("Could not confirm: {e}"),
            }
        }
        Err(e) => eprintln!("FAILED: {e}"),
    }
}

fn run_simulation(
    client: &RpcClient,
    keypair: &Keypair,
    state: &Pubkey,
    start: Option<u64>,
    step: u64,
    interval_ms: u64,
) {
    // Determine starting price
    let mut price = start.unwrap_or_else(|| {
        fetch_oracle_price(client, state).unwrap_or(150)
    });

    println!("Simulating price walk: start={price} max_step=±{step} interval={interval_ms}ms");
    println!("Press Ctrl+C to stop.");
    println!();

    // Simple LCG-based pseudo-random for portability (no rand dep needed)
    let mut rng_state: u64 = 0xdeadbeef_cafebabe;
    let lcg_next = |s: &mut u64| -> u64 {
        *s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        *s
    };

    let mut tick: u64 = 0;
    loop {
        tick += 1;

        // Random signed delta in [-step, +step]
        let rnd = lcg_next(&mut rng_state);
        let magnitude = (rnd % (step + 1)) as i64;
        let sign: i64 = if (rnd >> 63) == 1 { 1 } else { -1 };
        let delta = sign * magnitude;

        let new_price = if delta >= 0 {
            price.saturating_add(delta as u64)
        } else {
            price.saturating_sub((-delta) as u64).max(1)
        };

        print!("[tick={tick}] {price} → {new_price}  (Δ{delta:+})... ");
        io::stdout().flush().ok();

        match send_update_oracle(client, keypair, state, new_price) {
            Ok(sig) => println!("ok  ({})", &sig.to_string()[..8]),
            Err(e) => println!("FAILED: {e}"),
        }

        price = new_price;
        thread::sleep(Duration::from_millis(interval_ms));
    }
}

fn run_interactive(client: &RpcClient, keypair: &Keypair, state: &Pubkey) {
    println!("Interactive mode. Enter a new oracle price, or:");
    println!("  q / quit   — exit");
    println!("  s / show   — show current price");
    println!();

    let stdin = io::stdin();
    loop {
        print!("New price> ");
        io::stdout().flush().ok();

        let mut line = String::new();
        if stdin.lock().read_line(&mut line).is_err() {
            break;
        }
        let trimmed = line.trim();

        match trimmed.to_lowercase().as_str() {
            "" => continue,
            "q" | "quit" | "exit" => {
                println!("Goodbye.");
                break;
            }
            "s" | "show" | "status" => {
                match fetch_oracle_price(client, state) {
                    Ok(p) => println!("Current oracle price: {p}"),
                    Err(e) => eprintln!("Error: {e}"),
                }
            }
            s => match s.parse::<u64>() {
                Ok(0) => println!("Price must be > 0"),
                Ok(price) => set_price(client, keypair, state, price),
                Err(_) => println!("Invalid input — enter a positive integer or 'q' to quit"),
            },
        }
        println!();
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn fetch_oracle_price(
    client: &RpcClient,
    state_pubkey: &Pubkey,
) -> Result<u64, Box<dyn std::error::Error>> {
    let account = client.get_account(state_pubkey)?;
    if account.data.len() < std::mem::size_of::<PerpsState>() {
        return Err("state account too small".into());
    }
    let state: &PerpsState =
        bytemuck::try_from_bytes(&account.data).map_err(|e| format!("deserialize: {e}"))?;
    Ok(state.oracle_price)
}

fn send_update_oracle(
    client: &RpcClient,
    keypair: &Keypair,
    state: &Pubkey,
    price: u64,
) -> Result<String, Box<dyn std::error::Error>> {
    // Sync ix type 2 = UpdateOracle, param = new price
    let mut data = vec![0u8]; // route 0 = sync
    let ix_type: u64 = 2;
    data.extend_from_slice(&ix_type.to_le_bytes());
    data.extend_from_slice(&price.to_le_bytes());

    let ix = Instruction {
        program_id: PERPS_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*state, false),
            AccountMeta::new_readonly(keypair.pubkey(), true),
        ],
        data,
    };

    let blockhash = client.get_latest_blockhash()?;
    let tx = Transaction::new_signed_with_payer(
        &[ix],
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

    let interval_ms = arg_val(&args, "--interval-ms")
        .and_then(|s| s.parse().ok())
        .unwrap_or(2000);

    let mode = if let Some(price_str) = arg_val(&args, "--price") {
        let price = price_str.parse::<u64>().expect("--price must be a positive integer");
        Mode::Once(price)
    } else if args.iter().any(|a| a == "--simulate") {
        Mode::Simulate {
            start: arg_val(&args, "--sim-start").and_then(|s| s.parse().ok()),
            step: arg_val(&args, "--sim-step").and_then(|s| s.parse().ok()).unwrap_or(3),
            interval_ms,
        }
    } else {
        Mode::Interactive
    };

    Config {
        state: Pubkey::from_str(&state_str).expect("invalid state pubkey"),
        rpc_url: arg_val(&args, "--rpc")
            .or_else(|| std::env::var("PERPS_RPC_URL").ok())
            .unwrap_or_else(|| "https://api.devnet.solana.com".to_string()),
        keypair_path: arg_val(&args, "--keypair")
            .or_else(|| std::env::var("PERPS_KEYPAIR").ok())
            .unwrap_or_else(default_keypair_path),
        mode,
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
