use std::array::from_ref;
use std::path::Path;

use litesvm::LiteSVM;
use perps::state::PerpsState;
use sokoban::NodeAllocatorMap;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_program::clock::Clock;
use solana_program::message::Message;
use solana_program::system_instruction;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_transaction::Transaction;

const PERPS_PROGRAM_ID: Pubkey =
    solana_pubkey::pubkey!("PerpsProgram1111111111111111111111111111111");

fn main() {
    println!("=== ACE Perps: Cancel-Priority Perpetual Futures Demo ===\n");

    let mut svm = LiteSVM::new()
        .with_blockhash_check(false)
        .with_sigverify(false)
        .with_transaction_history(50);

    let path = Path::new("../target/deploy/perps.so");
    println!(
        "Loading program from: {} (exists: {})",
        path.display(),
        path.exists()
    );
    svm.add_program_from_file(PERPS_PROGRAM_ID, path).unwrap();
    let svm = &mut svm;

    let payer = Pubkey::new_unique();
    svm.airdrop(&payer, 10_000_000_000).unwrap();

    let state_account = Keypair::new();
    let state_size = std::mem::size_of::<PerpsState>();
    println!("State size: {} bytes\n", state_size);

    // Create state account
    let create_ix = system_instruction::create_account(
        &payer,
        &state_account.pubkey(),
        svm.minimum_balance_for_rent_exemption(state_size),
        state_size as u64,
        &PERPS_PROGRAM_ID,
    );
    execute(
        svm,
        &payer,
        from_ref(&create_ix),
        &[state_account.pubkey()],
        "Create state account",
    );

    // Users
    let market_maker = Pubkey::new_unique();
    let taker = Pubkey::new_unique();
    let oracle_authority = Pubkey::new_unique();

    println!("Market Maker: {}", short_pubkey(&market_maker));
    println!("Taker:        {}", short_pubkey(&taker));
    println!("Oracle:       {}", short_pubkey(&oracle_authority));

    // --- Step 1: Deposit collateral for both users ---
    println!("\n--- Step 1: Deposit collateral ---");
    let deposit_amount: u64 = 10_000_000; // 10M units

    let ix = create_sync_instruction(
        &state_account.pubkey(),
        &market_maker,
        0, // Deposit
        deposit_amount,
    );
    execute(svm, &payer, from_ref(&ix), &[], "MM deposits 10M collateral");

    let ix = create_sync_instruction(
        &state_account.pubkey(),
        &taker,
        0, // Deposit
        deposit_amount,
    );
    execute(svm, &payer, from_ref(&ix), &[], "Taker deposits 10M collateral");

    // --- Step 2: Set oracle price ---
    println!("\n--- Step 2: Set oracle price ---");
    let ix = create_sync_instruction(
        &state_account.pubkey(),
        &oracle_authority,
        2, // UpdateOracle
        150, // SOL at $150
    );
    execute(svm, &payer, from_ref(&ix), &[], "Oracle sets SOL price to $150");

    // --- Step 3: MM places a sell order (ask) ---
    println!("\n--- Step 3: MM places order on book ---");
    let ix = create_async_instruction(
        &state_account.pubkey(),
        &market_maker,
        2,   // PlaceOrder
        150, // price
        10,  // size (10 SOL)
        1,   // side: ask (sell)
        0,   // order_seq (will be assigned)
    );
    execute(svm, &payer, from_ref(&ix), &[], "MM queues ask: 10 SOL @ $150");

    // Advance slot and crank to place the order
    svm.warp_to_slot(get_current_slot(svm) + 2);
    let crank_ix = create_crank_instruction(&state_account.pubkey(), &payer);
    execute(svm, &payer, from_ref(&crank_ix), &[], "Crank: process MM's order placement");

    print_state(svm, &state_account.pubkey(), "After MM order placed");

    // --- Step 4: THE KEY TEST ---
    // MM submits cancel AND taker submits take IN THE SAME SLOT
    // Cancel should execute before take, protecting the MM
    println!("\n--- Step 4: Cancel vs Take race (same slot) ---");
    println!("MM submits cancel and Taker submits take in the SAME slot.");
    println!("ACE guarantees: cancel executes FIRST.\n");

    // The order that was placed gets seq from the queue processing.
    // We need to know the order_seq to cancel it. In the placed order, the order_seq
    // is whatever was assigned. For this demo, the order_seq in the queue was 0,
    // and it gets placed with seq=0 as the orderbook OrderId.seq.
    let placed_order_seq: u64 = 0;

    // Taker tries to buy (take) — queued FIRST in this tx batch
    let take_ix = create_async_instruction(
        &state_account.pubkey(),
        &taker,
        3,   // Take
        150, // limit price
        10,  // size
        0,   // side: buy
        0,
    );
    execute(svm, &payer, from_ref(&take_ix), &[], "Taker queues take: buy 10 SOL @ $150");

    // MM cancels — queued SECOND (but has higher priority!)
    let cancel_ix = create_async_instruction(
        &state_account.pubkey(),
        &market_maker,
        1,                 // Cancel
        150,               // price of order to cancel
        0,                 // size (unused for cancel)
        1,                 // side: ask
        placed_order_seq,  // order_seq of the order to cancel
    );
    execute(svm, &payer, from_ref(&cancel_ix), &[], "MM queues cancel for ask @ $150");

    print_state(svm, &state_account.pubkey(), "Before crank (both queued in same slot)");

    // Advance slot and crank
    svm.warp_to_slot(get_current_slot(svm) + 2);
    let crank_ix = create_crank_instruction(&state_account.pubkey(), &payer);
    execute(svm, &payer, from_ref(&crank_ix), &[], "Crank: process cancel + take");

    print_state(svm, &state_account.pubkey(), "After crank");

    // --- Verification ---
    println!("\n--- Verification ---");
    let account = svm.get_account(&state_account.pubkey()).unwrap();
    let state: &PerpsState = bytemuck::from_bytes(&account.data);

    // The orderbook should be empty (cancel removed the order)
    let has_asks = state.orderbook.asks.len() > 0;
    let has_bids = state.orderbook.bids.len() > 0;
    println!("Orderbook asks: {} (expected: 0)", state.orderbook.asks.len());
    println!("Orderbook bids: {} (expected: 0)", state.orderbook.bids.len());

    // The MM should NOT have a short position (cancel protected them)
    let mm_has_position = state.positions.find(&market_maker.to_bytes()).is_some();
    let taker_has_position = state.positions.find(&taker.to_bytes()).is_some();
    println!("MM has position: {} (expected: false — cancel protected MM!)", mm_has_position);
    println!("Taker has position: {} (expected: false — nothing to fill)", taker_has_position);

    if !has_asks && !has_bids && !mm_has_position {
        println!("\n=== SUCCESS: Cancel executed before Take! MM was protected! ===");
    } else {
        println!("\n=== FAILURE: Unexpected state ===");
    }

    // --- Bonus: Show what happens when cancel doesn't arrive ---
    println!("\n--- Bonus: Take succeeds when no cancel ---");

    // MM places another order
    let ix = create_async_instruction(
        &state_account.pubkey(),
        &market_maker,
        2, 150, 5, 1, 0,
    );
    execute(svm, &payer, from_ref(&ix), &[], "MM queues ask: 5 SOL @ $150");

    svm.warp_to_slot(get_current_slot(svm) + 2);
    let crank_ix = create_crank_instruction(&state_account.pubkey(), &payer);
    execute(svm, &payer, from_ref(&crank_ix), &[], "Crank: place order");

    // Now taker takes, no cancel this time
    let take_ix = create_async_instruction(
        &state_account.pubkey(),
        &taker,
        3, 150, 5, 0, 0,
    );
    execute(svm, &payer, from_ref(&take_ix), &[], "Taker queues take: buy 5 SOL @ $150");

    svm.warp_to_slot(get_current_slot(svm) + 2);
    let crank_ix = create_crank_instruction(&state_account.pubkey(), &payer);
    execute(svm, &payer, from_ref(&crank_ix), &[], "Crank: process take");

    print_state(svm, &state_account.pubkey(), "After successful fill");

    let account = svm.get_account(&state_account.pubkey()).unwrap();
    let state: &PerpsState = bytemuck::from_bytes(&account.data);
    let taker_pos = state.positions.find(&taker.to_bytes());
    let mm_pos = state.positions.find(&market_maker.to_bytes());
    if taker_pos.is_some() && mm_pos.is_some() {
        println!("\n=== SUCCESS: Fill executed correctly when no cancel! ===");
    }

    println!("\n=== Demo Complete ===");
}

fn get_current_slot(svm: &LiteSVM) -> u64 {
    svm.get_sysvar::<Clock>().slot
}

fn short_pubkey(pubkey: &Pubkey) -> String {
    pubkey.to_string()[..8].to_string()
}

/// Sync instruction: [0u8 route, ix_type: u64, param: u64]
fn create_sync_instruction(
    state_account: &Pubkey,
    user: &Pubkey,
    sync_ix: u64,
    param: u64,
) -> Instruction {
    let mut data = vec![0u8]; // route = 0 (sync)
    data.extend_from_slice(&sync_ix.to_le_bytes());
    data.extend_from_slice(&param.to_le_bytes());

    Instruction {
        program_id: PERPS_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*state_account, false),
            AccountMeta::new_readonly(*user, false),
        ],
        data,
    }
}

/// Async instruction: [1u8 route, ix_type: u64, price: u64, size: u64, side: u64, order_seq: u64]
fn create_async_instruction(
    state_account: &Pubkey,
    user: &Pubkey,
    async_ix: u64,
    price: u64,
    size: u64,
    side: u64,
    order_seq: u64,
) -> Instruction {
    let mut data = vec![1u8]; // route = 1 (async)
    data.extend_from_slice(&async_ix.to_le_bytes());
    data.extend_from_slice(&price.to_le_bytes());
    data.extend_from_slice(&size.to_le_bytes());
    data.extend_from_slice(&side.to_le_bytes());
    data.extend_from_slice(&order_seq.to_le_bytes());

    Instruction {
        program_id: PERPS_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*state_account, false),
            AccountMeta::new_readonly(*user, false),
        ],
        data,
    }
}

fn create_crank_instruction(state_account: &Pubkey, user: &Pubkey) -> Instruction {
    Instruction {
        program_id: PERPS_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*state_account, false),
            AccountMeta::new_readonly(*user, false),
        ],
        data: vec![2u8], // route = 2 (crank)
    }
}

#[track_caller]
fn execute(
    svm: &mut LiteSVM,
    &payer: &Pubkey,
    instructions: &[Instruction],
    additional_signers: &[Pubkey],
    description: &str,
) {
    if !description.is_empty() {
        println!("\n>> {}", description);
    }

    let mut signers = vec![payer];
    signers.extend_from_slice(additional_signers);

    let message = Message::new(instructions, Some(&payer));
    let tx = Transaction::new_unsigned(message);

    match svm.send_transaction(tx) {
        Ok(res) => {
            if !res.logs.is_empty() && !description.is_empty() {
                println!("   Logs:");
                for log in &res.logs {
                    println!("     {}", log);
                }
            }
        }
        Err(e) => {
            println!("called from {}", std::panic::Location::caller());
            panic!("   ERROR: {:?}", e);
        }
    }
}

fn print_state(svm: &LiteSVM, state_account: &Pubkey, context: &str) {
    println!("\n[State: {}]", context);

    if let Some(account) = svm.get_account(state_account) {
        let state: &PerpsState = bytemuck::from_bytes(&account.data);

        println!("  Seq: {}", state.seq);
        println!("  Oracle price: {}", state.oracle_price);
        println!("  Orderbook: {} bids, {} asks", state.orderbook.bids.len(), state.orderbook.asks.len());

        // Print margins
        for user in &state.margins.users {
            if user.is_active == 1 {
                let pk = Pubkey::new_from_array(user.owner);
                println!("  Margin {}: {}", short_pubkey(&pk), user.balance);
            }
        }

        // Print positions
        for pos in &state.positions.positions {
            if pos.is_active == 1 {
                let pk = Pubkey::new_from_array(pos.owner);
                let side_str = if pos.side == 0 { "LONG" } else { "SHORT" };
                println!(
                    "  Position {}: {} {} @ {}",
                    short_pubkey(&pk),
                    side_str,
                    pos.size,
                    pos.entry_price,
                );
            }
        }

        // Print queue
        let queue_len = state.async_queue.len();
        if queue_len > 0 {
            println!("  Queue ({} items):", queue_len);
            for (key, val) in state.async_queue.iter() {
                let ix_name = match val.ix_type {
                    0 => "Liquidate",
                    1 => "Cancel",
                    2 => "PlaceOrder",
                    3 => "Take",
                    _ => "Unknown",
                };
                let pk = Pubkey::new_from_array(val.user);
                println!(
                    "    slot={} pri={} seq={}: {} by {}",
                    key.slot,
                    key.priority,
                    key.seq,
                    ix_name,
                    short_pubkey(&pk),
                );
            }
        }
    } else {
        panic!("  Account not found!");
    }
}
