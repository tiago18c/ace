//! Setup: creates and initializes the perps state account on devnet.
//!
//! This is a one-time operation that must run before any other example.
//! It creates a new Solana account owned by the perps program, funds it for
//! rent-exemption, and sends an initial UpdateOracle instruction to trigger
//! the program's self-initialization (seq == 0 → initialize).
//!
//! The state account keypair is saved to disk so the address stays stable
//! across runs. By default it is written to ./perps-state.json.
//!
//! Usage:
//!   cargo run --example setup -- \
//!     [--rpc <URL>]                   (default: devnet)
//!     [--keypair <PATH>]              (payer, default: ~/.config/solana/id.json)
//!     [--state-keypair <PATH>]        (state acct keypair, default: ./perps-state.json)
//!     [--oracle-price <N>]            (initial oracle price, default: 150)
//!     [--force]                       (re-initialize even if account exists)
//!
//! After running, every other example can be pointed at the printed pubkey:
//!   PERPS_STATE_ACCOUNT=<PRINTED_PUBKEY> cargo run --example crank
//!
//! Environment variable alternatives:
//!   PERPS_RPC_URL, PERPS_KEYPAIR

use std::path::Path;

use perps::state::PerpsState;
use solana_commitment_config::CommitmentConfig;
use sokoban::NodeAllocatorMap;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_program::system_instruction;
use solana_pubkey::Pubkey;
use solana_rpc_client::rpc_client::RpcClient;
use solana_signer::Signer;
use solana_transaction::Transaction;

const PERPS_PROGRAM_ID: Pubkey =
    solana_pubkey::pubkey!("HBtR4MuDfC6unTEcC1buv5u6ubJ2yRxTpvFWtvAXKwQC");

struct Config {
    rpc_url: String,
    payer_path: String,
    state_keypair_path: String,
    oracle_price: u64,
    force: bool,
}

fn main() {
    let cfg = parse_config();
    let payer = load_keypair(&cfg.payer_path);
    let client = RpcClient::new_with_commitment(cfg.rpc_url.clone(), CommitmentConfig::confirmed());

    println!("=== ACE Perps Setup ===");
    println!("RPC endpoint        : {}", cfg.rpc_url);
    println!("Payer               : {}", payer.pubkey());
    println!("State keypair file  : {}", cfg.state_keypair_path);
    println!("Initial oracle price: {}", cfg.oracle_price);
    println!();

    // ── Load or generate the state keypair ────────────────────────────────────
    let state_keypair = if Path::new(&cfg.state_keypair_path).exists() {
        let kp = load_keypair(&cfg.state_keypair_path);
        println!("Loaded existing state keypair: {}", kp.pubkey());
        kp
    } else {
        let kp = Keypair::new();
        save_keypair(&kp, &cfg.state_keypair_path);
        println!("Generated new state keypair : {}", kp.pubkey());
        kp
    };

    let state_pubkey = state_keypair.pubkey();
    println!();

    // ── Check if the account already exists ───────────────────────────────────
    match client.get_account(&state_pubkey) {
        Ok(existing) => {
            if !cfg.force {
                println!("State account already exists on-chain.");
                println!("  Owner       : {}", existing.owner);
                println!("  Data length : {} bytes", existing.data.len());
                println!("  Lamports    : {}", existing.lamports);

                // Show initialization status
                if existing.data.len() >= std::mem::size_of::<PerpsState>() {
                    if let Ok(state) = bytemuck::try_from_bytes::<PerpsState>(&existing.data) {
                        if state.seq > 0 {
                            println!("  Status      : initialized (seq={})", state.seq);
                            println!("  Oracle price: {}", state.oracle_price);
                        } else {
                            println!("  Status      : allocated but NOT initialized (seq=0)");
                            println!("  Run with --force to re-send the init instruction.");
                        }
                    }
                }

                println!();
                print_usage(&state_pubkey);
                return;
            }
            println!("--force specified, proceeding with re-initialization.");
        }
        Err(_) => {
            println!("Account does not exist yet — creating.");
        }
    }

    // ── Verify payer has enough SOL ───────────────────────────────────────────
    let payer_balance = client
        .get_balance(&payer.pubkey())
        .unwrap_or_else(|e| panic!("cannot get payer balance: {e}"));
    println!("Payer balance: {} lamports  ({:.6} SOL)", payer_balance, payer_balance as f64 / 1e9);

    let state_size = std::mem::size_of::<PerpsState>();
    println!("State account size: {} bytes  ({:.2} KB)", state_size, state_size as f64 / 1024.0);

    let rent_lamports = client
        .get_minimum_balance_for_rent_exemption(state_size)
        .unwrap_or_else(|e| panic!("cannot get rent: {e}"));
    println!("Rent-exempt lamports: {} (~{:.4} SOL)", rent_lamports, rent_lamports as f64 / 1e9);

    // Rough fee estimate: 2 transactions × ~5000 lamports each
    let estimated_fees: u64 = 10_000;
    let total_needed = rent_lamports + estimated_fees;
    if payer_balance < total_needed {
        eprintln!(
            "\nERROR: payer needs at least {} lamports ({:.4} SOL) but has {}.",
            total_needed,
            total_needed as f64 / 1e9,
            payer_balance
        );
        eprintln!("Fund the payer with: solana airdrop 2 {} --url devnet", payer.pubkey());
        std::process::exit(1);
    }

    println!();

    // ── Step 1: Create the state account ─────────────────────────────────────
    // The account must be owned by PERPS_PROGRAM_ID so the program can write to it.
    println!("Step 1/2: Creating state account...");
    let create_ix = system_instruction::create_account(
        &payer.pubkey(),
        &state_pubkey,
        rent_lamports,
        state_size as u64,
        &PERPS_PROGRAM_ID,
    );

    let blockhash = client
        .get_latest_blockhash()
        .unwrap_or_else(|e| panic!("get blockhash: {e}"));

    // Both payer and the new state account must sign the create_account ix.
    let tx = Transaction::new_signed_with_payer(
        &[create_ix],
        Some(&payer.pubkey()),
        &[&payer, &state_keypair],
        blockhash,
    );

    let sig = client
        .send_and_confirm_transaction(&tx)
        .unwrap_or_else(|e| panic!("create_account failed: {e}"));
    println!("  Created: {sig}");

    // ── Step 2: Initialize by sending UpdateOracle ────────────────────────────
    // The program checks seq == 0 on first call and calls initialize_state().
    // UpdateOracle (sync ix type 2) is a natural first call that also sets a
    // useful initial price.
    println!("Step 2/2: Initializing state (UpdateOracle price={})...", cfg.oracle_price);

    let init_ix = sync_instruction(&state_pubkey, &payer.pubkey(), 2, cfg.oracle_price);

    let blockhash = client
        .get_latest_blockhash()
        .unwrap_or_else(|e| panic!("get blockhash: {e}"));
    let tx = Transaction::new_signed_with_payer(
        &[init_ix],
        Some(&payer.pubkey()),
        &[&payer],
        blockhash,
    );

    let sig = client
        .send_and_confirm_transaction(&tx)
        .unwrap_or_else(|e| panic!("init failed: {e}"));
    println!("  Initialized: {sig}");

    // ── Verify on-chain ───────────────────────────────────────────────────────
    println!();
    println!("Verifying state on-chain...");
    let account = client
        .get_account(&state_pubkey)
        .unwrap_or_else(|e| panic!("get account: {e}"));

    if account.data.len() >= std::mem::size_of::<PerpsState>() {
        if let Ok(state) = bytemuck::try_from_bytes::<PerpsState>(&account.data) {
            println!("  seq         : {}", state.seq);
            println!("  oracle_price: {}", state.oracle_price);
            println!("  queue depth : {}", state.async_queue.len());
            println!("  bid count   : {}", state.orderbook.bids.len());
            println!("  ask count   : {}", state.orderbook.asks.len());
        }
    }

    println!();
    println!("=== Setup complete ===");
    print_usage(&state_pubkey);
}

fn print_usage(state_pubkey: &Pubkey) {
    println!("State account pubkey:");
    println!("  {state_pubkey}");
    println!();
    println!("Use this pubkey with the other examples:");
    println!("  export PERPS_STATE_ACCOUNT={state_pubkey}");
    println!();
    println!("  cargo run --example orderbook_viewer -- --state {state_pubkey}");
    println!("  cargo run --example crank            -- --state {state_pubkey}");
    println!("  cargo run --example market_maker     -- --state {state_pubkey} --spread 5 --size 10");
    println!("  cargo run --example oracle           -- --state {state_pubkey} --price 155");
    println!("  cargo run --example margin           -- --state {state_pubkey} --deposit 1000000");
    println!("  cargo run --example taker            -- --state {state_pubkey}");
}

// ─── Instruction builder ──────────────────────────────────────────────────────

fn sync_instruction(state: &Pubkey, user: &Pubkey, ix_type: u64, param: u64) -> Instruction {
    let mut data = vec![0u8]; // route 0 = sync
    data.extend_from_slice(&ix_type.to_le_bytes());
    data.extend_from_slice(&param.to_le_bytes());
    Instruction {
        program_id: PERPS_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*state, false),
            AccountMeta::new_readonly(*user, true),
        ],
        data,
    }
}

// ─── Config / keypair helpers ─────────────────────────────────────────────────

fn parse_config() -> Config {
    let args: Vec<String> = std::env::args().collect();
    Config {
        rpc_url: arg_val(&args, "--rpc")
            .or_else(|| std::env::var("PERPS_RPC_URL").ok())
            .unwrap_or_else(|| "https://api.devnet.solana.com".to_string()),
        payer_path: arg_val(&args, "--keypair")
            .or_else(|| std::env::var("PERPS_KEYPAIR").ok())
            .unwrap_or_else(default_keypair_path),
        state_keypair_path: arg_val(&args, "--state-keypair")
            .unwrap_or_else(|| "./perps-state.json".to_string()),
        oracle_price: arg_val(&args, "--oracle-price")
            .and_then(|s| s.parse().ok())
            .unwrap_or(150),
        force: args.iter().any(|a| a == "--force"),
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

fn save_keypair(keypair: &Keypair, path: &str) {
    solana_keypair::write_keypair_file(keypair, path)
        .unwrap_or_else(|e| panic!("cannot save keypair to '{path}': {e}"));
    println!("Saved state keypair to: {path}");
}
