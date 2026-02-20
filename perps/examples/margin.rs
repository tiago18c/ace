//! Margin manager: deposit or withdraw collateral from the perps program.
//!
//! The caller's keypair acts as the user identity — the program credits/debits
//! the account that matches the second instruction account's pubkey.
//!
//! Modes:
//!   --deposit <N>        Deposit N units of collateral and exit.
//!   --withdraw <N>       Withdraw N units of collateral and exit.
//!   (neither)            Interactive: shows balance, prompts for action.
//!
//! Usage:
//!   cargo run --example margin -- \
//!     --state <STATE_PUBKEY>      (required)
//!     [--deposit <N>]
//!     [--withdraw <N>]
//!     [--rpc <URL>]               (default: devnet)
//!     [--keypair <PATH>]          (default: ~/.config/solana/id.json)
//!
//! Environment variable alternatives:
//!   PERPS_STATE_ACCOUNT, PERPS_RPC_URL, PERPS_KEYPAIR

use std::io::{self, BufRead, Write};
use std::str::FromStr;

use perps::state::PerpsState;
use solana_commitment_config::CommitmentConfig;
use sokoban::NodeAllocatorMap;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_rpc_client::rpc_client::RpcClient;
use solana_signer::Signer;
use solana_transaction::Transaction;

const PERPS_PROGRAM_ID: Pubkey =
    solana_pubkey::pubkey!("HBtR4MuDfC6unTEcC1buv5u6ubJ2yRxTpvFWtvAXKwQC");

enum Action {
    Deposit(u64),
    Withdraw(u64),
    Interactive,
}

struct Config {
    state: Pubkey,
    rpc_url: String,
    keypair_path: String,
    action: Action,
}

fn main() {
    let cfg = parse_config();
    let keypair = load_keypair(&cfg.keypair_path);
    let client = RpcClient::new_with_commitment(cfg.rpc_url.clone(), CommitmentConfig::confirmed());
    let my_bytes: [u8; 32] = keypair.pubkey().to_bytes();

    println!("=== ACE Perps Margin Manager ===");
    println!("State account: {}", cfg.state);
    println!("User pubkey  : {}", keypair.pubkey());
    println!("RPC endpoint : {}", cfg.rpc_url);
    println!();

    // Show current balance and state summary
    print_user_status(&client, &cfg.state, &my_bytes);
    println!();

    match cfg.action {
        Action::Deposit(amount) => {
            execute_deposit(&client, &keypair, &cfg.state, amount);
            println!();
            print_user_status(&client, &cfg.state, &my_bytes);
        }
        Action::Withdraw(amount) => {
            execute_withdraw(&client, &keypair, &cfg.state, amount);
            println!();
            print_user_status(&client, &cfg.state, &my_bytes);
        }
        Action::Interactive => {
            run_interactive(&client, &keypair, &cfg.state, &my_bytes);
        }
    }
}

// ─── Actions ─────────────────────────────────────────────────────────────────

fn execute_deposit(client: &RpcClient, keypair: &Keypair, state: &Pubkey, amount: u64) {
    if amount == 0 {
        println!("Deposit amount must be > 0");
        return;
    }
    print!("Depositing {amount}... ");
    io::stdout().flush().ok();
    match send_sync_ix(client, keypair, state, 0, amount) {
        Ok(sig) => {
            println!("ok");
            println!("Signature: {sig}");
        }
        Err(e) => eprintln!("FAILED: {e}"),
    }
}

fn execute_withdraw(client: &RpcClient, keypair: &Keypair, state: &Pubkey, amount: u64) {
    if amount == 0 {
        println!("Withdraw amount must be > 0");
        return;
    }
    print!("Withdrawing {amount}... ");
    io::stdout().flush().ok();
    match send_sync_ix(client, keypair, state, 1, amount) {
        Ok(sig) => {
            println!("ok");
            println!("Signature: {sig}");
        }
        Err(e) => eprintln!("FAILED: {e}"),
    }
}

fn run_interactive(client: &RpcClient, keypair: &Keypair, state: &Pubkey, my_bytes: &[u8; 32]) {
    println!("Interactive mode. Commands:");
    println!("  deposit <N>    — deposit N units");
    println!("  withdraw <N>   — withdraw N units");
    println!("  balance / b    — refresh balance");
    println!("  positions / p  — show open positions");
    println!("  q / quit       — exit");
    println!();

    let stdin = io::stdin();
    loop {
        print!("> ");
        io::stdout().flush().ok();

        let mut line = String::new();
        if stdin.lock().read_line(&mut line).is_err() {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let parts: Vec<&str> = trimmed.splitn(2, ' ').collect();
        match parts[0].to_lowercase().as_str() {
            "q" | "quit" | "exit" => {
                println!("Goodbye.");
                break;
            }
            "b" | "bal" | "balance" | "status" => {
                print_user_status(client, state, my_bytes);
            }
            "p" | "pos" | "positions" => {
                print_positions(client, state, my_bytes);
            }
            "deposit" | "d" => {
                if let Some(amt_str) = parts.get(1) {
                    match amt_str.trim().parse::<u64>() {
                        Ok(0) | Err(_) => println!("Usage: deposit <positive integer>"),
                        Ok(amount) => {
                            execute_deposit(client, keypair, state, amount);
                            println!();
                            print_user_status(client, state, my_bytes);
                        }
                    }
                } else {
                    println!("Usage: deposit <amount>");
                }
            }
            "withdraw" | "w" => {
                if let Some(amt_str) = parts.get(1) {
                    match amt_str.trim().parse::<u64>() {
                        Ok(0) | Err(_) => println!("Usage: withdraw <positive integer>"),
                        Ok(amount) => {
                            execute_withdraw(client, keypair, state, amount);
                            println!();
                            print_user_status(client, state, my_bytes);
                        }
                    }
                } else {
                    println!("Usage: withdraw <amount>");
                }
            }
            other => println!("Unknown command '{other}'. Type 'q' to quit."),
        }
        println!();
    }
}

// ─── Display helpers ──────────────────────────────────────────────────────────

fn print_user_status(client: &RpcClient, state_pubkey: &Pubkey, my_bytes: &[u8; 32]) {
    let Ok(account) = client.get_account(state_pubkey) else {
        eprintln!("Cannot fetch state account");
        return;
    };
    if account.data.len() < std::mem::size_of::<PerpsState>() {
        eprintln!("State account too small");
        return;
    }
    let Ok(state) = bytemuck::try_from_bytes::<PerpsState>(&account.data) else {
        eprintln!("Cannot deserialize state");
        return;
    };

    let balance = state.margins.get_balance(my_bytes);
    println!("Balance         : {balance}");
    println!("Oracle price    : {}", state.oracle_price);

    // Show open position if any
    if let Some(idx) = state.positions.find(my_bytes) {
        let pos = &state.positions.positions[idx];
        let side_str = if pos.side == 0 { "LONG" } else { "SHORT" };
        let (upnl_abs, is_pos) = pos.unrealized_pnl(state.oracle_price);
        let upnl_str = if is_pos {
            format!("+{upnl_abs}")
        } else {
            format!("-{upnl_abs}")
        };
        let notional = state.oracle_price * pos.size;
        let liq = pos.is_liquidatable(state.oracle_price, balance, 500);

        println!("Open position   : {side_str} {size} @ {entry}",
            size = pos.size, entry = pos.entry_price);
        println!("  Notional      : {notional}");
        println!("  Unrealized PnL: {upnl_str}");
        println!("  Margin        : {balance}");
        println!("  Liquidatable  : {liq}");
    } else {
        println!("Open position   : none");
    }

    // Show any queued instructions belonging to this user
    let my_queued: Vec<_> = state
        .async_queue
        .iter()
        .filter(|(_, q)| &q.user == my_bytes)
        .collect();

    if !my_queued.is_empty() {
        println!("Queued orders   : {}", my_queued.len());
        for (key, q) in &my_queued {
            let type_name = match q.ix_type {
                0 => "Liquidate",
                1 => "Cancel",
                2 => "PlaceOrder",
                3 => "Take",
                _ => "Unknown",
            };
            let side_str = if q.side == 0 { "bid/buy" } else { "ask/sell" };
            println!(
                "  slot={} pri={} → {type_name} {side_str} size={} @ {}",
                key.slot, key.priority, q.size, q.price
            );
        }
    }
}

fn print_positions(client: &RpcClient, state_pubkey: &Pubkey, my_bytes: &[u8; 32]) {
    let Ok(account) = client.get_account(state_pubkey) else {
        eprintln!("Cannot fetch state account");
        return;
    };
    if account.data.len() < std::mem::size_of::<PerpsState>() {
        return;
    }
    let Ok(state) = bytemuck::try_from_bytes::<PerpsState>(&account.data) else {
        return;
    };

    // Show own position
    match state.positions.find(my_bytes) {
        None => println!("No open position."),
        Some(idx) => {
            let pos = &state.positions.positions[idx];
            let side_str = if pos.side == 0 { "LONG" } else { "SHORT" };
            let (upnl_abs, is_pos) = pos.unrealized_pnl(state.oracle_price);
            let upnl_sign = if is_pos { "+" } else { "-" };
            let balance = state.margins.get_balance(my_bytes);
            let liq = pos.is_liquidatable(state.oracle_price, balance, 500);
            println!("Position: {side_str}");
            println!("  Size         : {}", pos.size);
            println!("  Entry price  : {}", pos.entry_price);
            println!("  Oracle price : {}", state.oracle_price);
            println!("  UPnL         : {upnl_sign}{upnl_abs}");
            println!("  Margin       : {balance}");
            println!("  Liquidatable : {liq}");
        }
    }
}

// ─── Instruction builder ──────────────────────────────────────────────────────

fn send_sync_ix(
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

    // --deposit and --withdraw are mutually exclusive; --deposit wins if both given
    let action = if let Some(amt) = arg_val(&args, "--deposit").and_then(|s| s.parse::<u64>().ok()) {
        Action::Deposit(amt)
    } else if let Some(amt) = arg_val(&args, "--withdraw").and_then(|s| s.parse::<u64>().ok()) {
        Action::Withdraw(amt)
    } else {
        Action::Interactive
    };

    Config {
        state: Pubkey::from_str(&state_str).expect("invalid state pubkey"),
        rpc_url: arg_val(&args, "--rpc")
            .or_else(|| std::env::var("PERPS_RPC_URL").ok())
            .unwrap_or_else(|| "https://api.devnet.solana.com".to_string()),
        keypair_path: arg_val(&args, "--keypair")
            .or_else(|| std::env::var("PERPS_KEYPAIR").ok())
            .unwrap_or_else(default_keypair_path),
        action,
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
