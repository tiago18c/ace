//! Orderbook viewer: polls devnet every N ms and prints a formatted view of the
//! live orderbook, pending queue, and the effective orderbook that combines both.
//!
//! The "effective" orderbook is what a rational participant should price against:
//!   - Live bids/asks currently on the book
//!   - Plus queued PlaceOrders that haven't been cranked yet
//!   - Minus queued Cancels (orders that are about to disappear)
//!   - Minus queued Takes (market orders that will consume resting liquidity)
//!
//! Usage:
//!   cargo run --example orderbook_viewer -- \
//!     --state <STATE_PUBKEY>      (required)
//!     [--rpc   <URL>]             (default: devnet)
//!     [--interval-ms <N>]         (default: 400)
//!
//! Environment variable alternatives:
//!   PERPS_STATE_ACCOUNT, PERPS_RPC_URL, PERPS_INTERVAL_MS

use std::collections::BTreeMap;
use std::str::FromStr;
use std::thread;
use std::time::{Duration, Instant};

use perps::state::PerpsState;
use solana_commitment_config::CommitmentConfig;
use solana_pubkey::Pubkey;
use solana_rpc_client::rpc_client::RpcClient;
use sokoban::NodeAllocatorMap;

struct Config {
    state: Pubkey,
    rpc_url: String,
    interval_ms: u64,
}

fn main() {
    let cfg = parse_config();
    let client = RpcClient::new_with_commitment(cfg.rpc_url.clone(), CommitmentConfig::confirmed());

    println!("=== ACE Perps Orderbook Viewer ===");
    println!("State account: {}", cfg.state);
    println!("RPC endpoint : {}", cfg.rpc_url);
    println!("Refresh rate : {}ms", cfg.interval_ms);
    println!();

    let mut refresh: u64 = 0;
    loop {
        let t0 = Instant::now();
        match render(&client, &cfg.state, refresh) {
            Ok(()) => {}
            Err(e) => eprintln!("[viewer error] {e}"),
        }
        refresh += 1;
        let elapsed = t0.elapsed().as_millis() as u64;
        let sleep = cfg.interval_ms.saturating_sub(elapsed);
        thread::sleep(Duration::from_millis(sleep));
    }
}

fn render(
    client: &RpcClient,
    state_pubkey: &Pubkey,
    refresh: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let account = client.get_account(state_pubkey)?;
    if account.data.len() < std::mem::size_of::<PerpsState>() {
        return Err("state account too small".into());
    }
    let state: &PerpsState =
        bytemuck::try_from_bytes(&account.data).map_err(|e| format!("deserialize: {e}"))?;

    let slot = client.get_slot()?;

    // ── Collect live bids ──────────────────────────────────────────────────────
    // Bids are stored with inverted price_key (u64::MAX - price) so that
    // min-traversal of the RBTree gives the best bid (highest real price).
    // We un-invert here for display.
    let mut live_bids: Vec<(u64, u64, [u8; 32], u64)> = state
        .orderbook
        .bids
        .iter()
        .map(|(key, order)| {
            let real_price = u64::MAX - key.price_key;
            (real_price, order.size, order.owner, key.seq)
        })
        .collect();
    live_bids.sort_by(|a, b| b.0.cmp(&a.0).then(a.3.cmp(&b.3))); // highest price first

    // ── Collect live asks ──────────────────────────────────────────────────────
    let mut live_asks: Vec<(u64, u64, [u8; 32], u64)> = state
        .orderbook
        .asks
        .iter()
        .map(|(key, order)| (key.price_key, order.size, order.owner, key.seq))
        .collect();
    live_asks.sort_by(|a, b| a.0.cmp(&b.0).then(a.3.cmp(&b.3))); // lowest price first

    // ── Collect pending queue items ────────────────────────────────────────────
    let mut queued: Vec<_> = state.async_queue.iter().collect();
    queued.sort_by_key(|(k, _)| (k.slot, k.priority, k.seq));

    // Classify queue items by type for display and effective-OB computation
    let mut queued_bids: Vec<(u64, u64, [u8; 32])> = Vec::new(); // pending PlaceOrder bids
    let mut queued_asks: Vec<(u64, u64, [u8; 32])> = Vec::new(); // pending PlaceOrder asks
    // Cancels: (price, order_seq, side) — used to mark orders that will disappear
    let mut queued_cancel_asks: Vec<(u64, u64)> = Vec::new(); // (price, order_seq)
    let mut queued_cancel_bids: Vec<(u64, u64)> = Vec::new();
    // Takes: (side, limit_price, size) in queue order — will eat resting liquidity.
    // side=0 buys (eats asks at price ≤ limit_price), side=1 sells (eats bids at price ≥ limit_price).
    let mut queued_takes: Vec<(u64, u64, u64)> = Vec::new();

    for (_, q) in &queued {
        match q.ix_type {
            2 => {
                // PlaceOrder
                if q.side == 0 {
                    queued_bids.push((q.price, q.size, q.user));
                } else {
                    queued_asks.push((q.price, q.size, q.user));
                }
            }
            1 => {
                // Cancel
                if q.side == 0 {
                    queued_cancel_bids.push((q.price, q.order_seq));
                } else {
                    queued_cancel_asks.push((q.price, q.order_seq));
                }
            }
            3 => {
                // Take — queue is already sorted by (slot, priority, seq) so order is preserved
                queued_takes.push((q.side, q.price, q.size));
            }
            _ => {}
        }
    }

    // ── Effective orderbook ────────────────────────────────────────────────────
    // Aggregate by price level: live + queued additions - queued removals.
    // key = price, value = net size delta
    let mut eff_bids: BTreeMap<u64, i64> = BTreeMap::new();
    let mut eff_asks: BTreeMap<u64, i64> = BTreeMap::new();

    for (price, size, _, _) in &live_bids {
        *eff_bids.entry(*price).or_default() += *size as i64;
    }
    for (price, size, _) in &queued_bids {
        *eff_bids.entry(*price).or_default() += *size as i64;
    }
    // Subtract cancelled bids (match by price — imprecise but best we can do
    // without knowing the exact size of the cancelled order; use live order size)
    for (cancel_price, cancel_seq) in &queued_cancel_bids {
        if let Some((_, size, _, _)) = live_bids.iter().find(|(p, _, _, s)| p == cancel_price && s == cancel_seq) {
            *eff_bids.entry(*cancel_price).or_default() -= *size as i64;
        }
    }

    for (price, size, _, _) in &live_asks {
        *eff_asks.entry(*price).or_default() += *size as i64;
    }
    for (price, size, _) in &queued_asks {
        *eff_asks.entry(*price).or_default() += *size as i64;
    }
    for (cancel_price, cancel_seq) in &queued_cancel_asks {
        if let Some((_, size, _, _)) = live_asks.iter().find(|(p, _, _, s)| p == cancel_price && s == cancel_seq) {
            *eff_asks.entry(*cancel_price).or_default() -= *size as i64;
        }
    }

    // Apply queued takes in queue-priority order (Liquidate < Cancel < PlaceOrder < Take,
    // so takes run after all cancels and placements within a slot have been processed).
    // A buy take (side=0) sweeps asks from lowest price up to limit_price.
    // A sell take (side=1) sweeps bids from highest price down to limit_price.
    for &(side, limit_price, size) in &queued_takes {
        let mut remaining = size;
        if side == 0 {
            for (price, eff_size) in eff_asks.iter_mut() {
                if remaining == 0 || *price > limit_price {
                    break;
                }
                let consume = remaining.min((*eff_size).max(0) as u64);
                *eff_size -= consume as i64;
                remaining -= consume;
            }
        } else {
            for (price, eff_size) in eff_bids.iter_mut().rev() {
                if remaining == 0 || *price < limit_price {
                    break;
                }
                let consume = remaining.min((*eff_size).max(0) as u64);
                *eff_size -= consume as i64;
                remaining -= consume;
            }
        }
    }

    // ── Compute spread ─────────────────────────────────────────────────────────
    let best_bid = live_bids.first().map(|(p, _, _, _)| *p);
    let best_ask = live_asks.first().map(|(p, _, _, _)| *p);
    let spread = match (best_bid, best_ask) {
        (Some(b), Some(a)) if a >= b => Some(a - b),
        _ => None,
    };

    // ── Expected fills simulation ──────────────────────────────────────────────
    // Replay every queued instruction in execution order (slot → priority → seq)
    // so we can predict which takes will fill and against which resting orders.
    //
    // Within a slot the crank runs: Cancel(1) → PlaceOrder(2) → Take(3).
    // Since `queued` is already sorted by (slot, priority, seq), iterating it
    // in order is the correct execution sequence — a cancel really does run
    // before the take in the same slot, so a cancelled order won't be matched.
    //
    // sim_bids key: (u64::MAX − price, seq) → ascending key = descending real price.
    let mut sim_asks: BTreeMap<(u64, u64), (u64, [u8; 32])> = BTreeMap::new(); // (size, owner)
    let mut sim_bids: BTreeMap<(u64, u64), (u64, u64, [u8; 32])> = BTreeMap::new(); // (real_price, size, owner)

    for (price, size, owner, seq) in &live_asks {
        sim_asks.insert((*price, *seq), (*size, *owner));
    }
    for (price, size, owner, seq) in &live_bids {
        sim_bids.insert((u64::MAX - price, *seq), (*price, *size, *owner));
    }

    // (taker, side, limit_price, req_size, fills: Vec<(fill_price, fill_size, maker)>)
    let mut take_results: Vec<([u8; 32], u64, u64, u64, Vec<(u64, u64, [u8; 32])>)> = Vec::new();

    for (_, q) in &queued {
        match q.ix_type {
            1 => {
                // Cancel: remove the specific order from the sim book.
                if q.side == 0 {
                    sim_bids.remove(&(u64::MAX - q.price, q.order_seq));
                } else {
                    sim_asks.remove(&(q.price, q.order_seq));
                }
            }
            2 => {
                // PlaceOrder: insert into sim book so subsequent takes can match it.
                if q.side == 0 {
                    sim_bids.insert(
                        (u64::MAX - q.price, q.order_seq),
                        (q.price, q.size, q.user),
                    );
                } else {
                    sim_asks.insert((q.price, q.order_seq), (q.size, q.user));
                }
            }
            3 => {
                // Take: match greedily against the sim book as it stands right now.
                let mut remaining = q.size;
                let mut fills: Vec<(u64, u64, [u8; 32])> = Vec::new();

                if q.side == 0 {
                    // Buy: sweep asks from lowest price up to limit_price.
                    let ask_keys: Vec<(u64, u64)> = sim_asks.keys().copied().collect();
                    for key in ask_keys {
                        if remaining == 0 || key.0 > q.price { break; }
                        let (size, maker) = *sim_asks.get(&key).unwrap();
                        let fill_size = remaining.min(size);
                        fills.push((key.0, fill_size, maker));
                        remaining -= fill_size;
                        if fill_size >= size {
                            sim_asks.remove(&key);
                        } else {
                            sim_asks.get_mut(&key).unwrap().0 -= fill_size;
                        }
                    }
                } else {
                    // Sell: sweep bids from highest price down to limit_price.
                    let bid_keys: Vec<(u64, u64)> = sim_bids.keys().copied().collect();
                    for key in bid_keys {
                        if remaining == 0 { break; }
                        let (real_price, size, maker) = *sim_bids.get(&key).unwrap();
                        if real_price < q.price { break; }
                        let fill_size = remaining.min(size);
                        fills.push((real_price, fill_size, maker));
                        remaining -= fill_size;
                        if fill_size >= size {
                            sim_bids.remove(&key);
                        } else {
                            sim_bids.get_mut(&key).unwrap().1 -= fill_size;
                        }
                    }
                }

                take_results.push((q.user, q.side, q.price, q.size, fills));
            }
            _ => {}
        }
    }

    // ── Render ─────────────────────────────────────────────────────────────────
    // Clear screen with ANSI escape for a ticker-like display
    if refresh > 0 {
        print!("\x1B[2J\x1B[1;1H");
    }

    println!("━━━ ACE Perps Orderbook ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!(
        "  Slot: {slot}   Oracle: {}   Queue depth: {}",
        state.oracle_price,
        state.async_queue.len()
    );
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");

    // ── Live orderbook section ─────────────────────────────────────────────────
    println!("\n  LIVE ORDERBOOK  ({} bids, {} asks)", live_bids.len(), live_asks.len());
    println!("  {:>10}  {:>8}  {:<8}  Owner", "Price", "Size", "Side");
    println!("  {}", "-".repeat(52));

    for (price, size, owner, _) in live_asks.iter().rev().take(10) {
        println!(
            "  {:>10}  {:>8}  {:<8}  {}",
            price,
            size,
            "ASK",
            short_key(owner)
        );
    }

    match spread {
        Some(s) => println!("  ─── spread: {s} ───────────────────────────────────────"),
        None => println!("  ─── no spread (empty book) ────────────────────────────"),
    }

    for (price, size, owner, _) in live_bids.iter().take(10) {
        println!(
            "  {:>10}  {:>8}  {:<8}  {}",
            price,
            size,
            "BID",
            short_key(owner)
        );
    }

    // ── Pending queue section ──────────────────────────────────────────────────
    if !queued.is_empty() {
        println!("\n  PENDING QUEUE  ({} items)", queued.len());
        println!("  {:>6}  {:>4}  {:>4}  {:<10}  {:>8}  {:>6}  Owner", "Slot", "Pri", "Seq", "Type", "Price", "Size");
        println!("  {}", "-".repeat(72));

        for (key, val) in queued.iter().take(20) {
            let status = if key.slot + 1 <= slot { "READY" } else { "wait" };
            println!(
                "  {:>6}  {:>4}  {:>4}  {:<10}  {:>8}  {:>6}  {}  [{status}]",
                key.slot,
                key.priority,
                key.seq,
                ix_type_name(val.ix_type),
                val.price,
                val.size,
                short_key(&val.user),
            );
        }
        if queued.len() > 20 {
            println!("  ... ({} more)", queued.len() - 20);
        }
    }

    // ── Effective orderbook section ────────────────────────────────────────────
    let eff_ask_levels: Vec<_> = eff_asks.iter().filter(|(_, s)| **s > 0).collect();
    let eff_bid_levels: Vec<_> = eff_bids.iter().rev().filter(|(_, s)| **s > 0).collect();

    if !eff_ask_levels.is_empty() || !eff_bid_levels.is_empty() {
        println!("\n  EFFECTIVE ORDERBOOK  (live + queued placements − queued cancels − queued takes)");
        println!("  {:>10}  {:>8}  {:<5}", "Price", "Size", "Side");
        println!("  {}", "-".repeat(32));

        for (price, size) in eff_ask_levels.iter().rev().take(10) {
            let tag = if live_asks.iter().any(|(p, _, _, _)| p == *price) { "" } else { " +" };
            println!("  {:>10}  {:>8}  ASK{tag}", price, size);
        }

        let eff_best_bid = eff_bid_levels.iter().next().map(|(p, _)| **p);
        let eff_best_ask = eff_ask_levels.iter().next().map(|(p, _)| **p);
        match (eff_best_bid, eff_best_ask) {
            (Some(b), Some(a)) if a >= b => {
                println!("  ─── effective spread: {} ─────────────────", a - b)
            }
            _ => println!("  ──────────────────────────────────────────────"),
        }

        for (price, size) in eff_bid_levels.iter().take(10) {
            let tag = if live_bids.iter().any(|(p, _, _, _)| p == *price) { "" } else { " +" };
            println!("  {:>10}  {:>8}  BID{tag}", price, size);
        }
    }

    // ── Active positions / margins ─────────────────────────────────────────────
    let active_positions: Vec<_> = state
        .positions
        .positions
        .iter()
        .filter(|p| p.is_active == 1)
        .collect();

    if !active_positions.is_empty() {
        println!("\n  POSITIONS  ({} open)", active_positions.len());
        println!("  {:<8}  {:<6}  {:>8}  {:>12}  Owner", "Side", "Size", "Entry", "UPnL@oracle");
        println!("  {}", "-".repeat(60));
        for pos in &active_positions {
            let side = if pos.side == 0 { "LONG" } else { "SHORT" };
            let (upnl_abs, upnl_pos) = pos.unrealized_pnl(state.oracle_price);
            let upnl_str = if upnl_pos {
                format!("+{upnl_abs}")
            } else {
                format!("-{upnl_abs}")
            };
            let owner = short_key(&pos.owner);
            println!(
                "  {:<8}  {:>6}  {:>8}  {:>12}  {}",
                side, pos.size, pos.entry_price, upnl_str, owner
            );
        }
    }

    // ── Expected fills section ─────────────────────────────────────────────────
    if !queued.is_empty() {
        let total_fills: usize = take_results.iter().map(|(_, _, _, _, fills)| fills.len()).sum();
        println!(
            "\n  EXPECTED FILLS  ({} takes, {} fills)",
            take_results.len(),
            total_fills,
        );

        if take_results.is_empty() {
            println!("  (no takes in queue — pending ops are cancels/placements only, no fills expected)");
        } else {
            println!("  {:<8}  {:<13}  Detail", "Taker", "Action");
            println!("  {}", "-".repeat(70));

            for (taker, side, limit_price, req_size, fills) in &take_results {
                // "BUY@<=100" or "SELL@>=90"
                let action = if *side == 0 {
                    format!("BUY@<={limit_price}")
                } else {
                    format!("SELL@>={limit_price}")
                };

                let filled: u64 = fills.iter().map(|(_, s, _)| s).sum();

                // "filled/req: size@price(maker), ..." or "0/req: no fill"
                let detail = if fills.is_empty() {
                    format!("0/{req_size}: no fill")
                } else {
                    let fills_str = fills
                        .iter()
                        .map(|(fp, fs, maker)| format!("{fs}@{fp}({})", short_key(maker)))
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("{filled}/{req_size}: {fills_str}")
                };

                println!("  {:<8}  {:<13}  {}", short_key(taker), action, detail);
            }
        }
    }

    println!(
        "\n  Last update: {:?}  (refresh #{})",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        refresh
    );

    Ok(())
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

fn short_key(bytes: &[u8; 32]) -> String {
    let pk = Pubkey::new_from_array(*bytes);
    pk.to_string()[..8].to_string()
}

// ─── Config helpers ───────────────────────────────────────────────────────────

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
        interval_ms: arg_val(&args, "--interval-ms")
            .or_else(|| std::env::var("PERPS_INTERVAL_MS").ok())
            .and_then(|s| s.parse().ok())
            .unwrap_or(400),
    }
}

fn arg_val(args: &[String], flag: &str) -> Option<String> {
    let pos = args.iter().position(|a| a == flag)?;
    args.get(pos + 1).cloned()
}
