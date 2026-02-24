> [!WARNING]
> **This is a toy/research project. It is unaudited, incomplete, and not suitable for production use. Do not deploy with real funds.**

# ACE Perps: Cancel-Priority Perpetual Futures

Perpetual futures implementation using the ACE (Application Controlled Execution) pattern to solve the cancel/take race condition that plagues on-chain perps.

## The Problem

On-chain perps have a fundamental fairness issue. Market makers post limit orders and cancel them as prices move. Takers (and MEV bots) race to fill those stale quotes before the cancel lands. Whoever wins depends on transaction ordering within a slot — essentially random or MEV-biddable. This forces MMs to widen spreads, making on-chain perps uncompetitive.

## The Solution

ACE enforces **deterministic priority ordering at the program level**. All instructions within a slot are collected into a priority queue (Red-Black Tree), sorted by `(slot, instruction_type, sequence_number)`:

| Priority | Instruction | Rationale |
|----------|-------------|-----------|
| 0 | Liquidate | Protect protocol solvency |
| 1 | Cancel | Protect market makers |
| 2 | PlaceOrder | Add liquidity |
| 3 | Take | Consume liquidity |

Within the same priority class, instructions execute FIFO by submission order.

A **cranker** processes the queue after each slot, executing instructions in priority order atomically. Cancels always execute before takes. No Jito dependency, no external infra — enforced by the program itself.

## How It Works

```
Slot N:   MM submits Cancel, Taker submits Take (any order)
          Both land in the async queue, sorted by priority

Slot N+1: Cranker calls process_async
          → Cancel executes first (priority 1), order removed from book
          → Take executes second (priority 3), finds nothing to fill
          → MM protected
```

Sync instructions (deposit, withdraw, oracle updates) execute immediately — only contentious orderbook operations go through the queue.

## Architecture

```
perps/src/
├── lib.rs                  # Entrypoint, Program impl, AsyncState impl
├── state.rs                # PerpsState — orderbook + positions + queue in one account
├── orderbook.rs            # Bid/ask book using sokoban RedBlackTree
├── positions.rs            # Position tracking, margin balances
├── sync_instructions.rs    # Deposit, Withdraw, UpdateOracle, ApplyFunding
├── async_instructions.rs   # Cancel, PlaceOrder, Take, Liquidate (priority-ordered)
└── settlement.rs           # Fill matching, PnL calculation, liquidation
```

**Instruction routing** (byte 0 of instruction data):
- `0` → Sync instruction (immediate)
- `1` → Queue async instruction (inserted into priority queue)
- `2` → Crank (process all pending instructions from previous slots)

## Run It

```bash
# Build the on-chain program
cargo-build-sbf

# Run unit tests (priority ordering, settlement math, margin)
cargo test -p perps

# Run the LiteSVM demo
cd perps && cargo run --example perps_demo
```

The demo walks through:
1. Deposit collateral for MM and taker
2. MM places a sell order
3. MM cancels + taker takes in the **same slot**
4. Crank processes → cancel first, take gets nothing
5. Then shows a normal fill when no cancel is submitted

## Tradeoffs

**1-slot latency (~400ms).** Instructions can't execute in the same slot they're submitted — the queue needs all of a slot's instructions before it can sort them. This is fundamental to the fairness guarantee.

| System | Latency | Cancel priority |
|--------|---------|-----------------|
| **ACE perps** | ~400ms | Provably guaranteed |
| Phoenix v1 | 0ms | No guarantee (race) |
| Drift JIT | ~5s | Moderate (auction) |
| CEX | ~1-10ms | Guaranteed (sequencer) |

**Single-account contention.** The current design puts everything in one account (simple for demo). Production would shard by market or split queue/book/positions into separate accounts.

## Examples

Run from the `perps` directory with `cargo run --example <name>`:

| Example | Description |
|---|---|
| `setup` | Initialize program accounts and state |
| `oracle` | Update the oracle price |
| `margin` | Deposit or withdraw margin |
| `market_maker` | Place and manage resting orders |
| `taker` | Submit take orders against the book |
| `crank` | Process pending async instructions |
| `orderbook_viewer` | Print the current orderbook state |
| `perps_demo` | End-to-end demo: setup, quote, take, crank |
| `malicious_market_maker` | Demonstrates shred-reactive spoofing (see analysis below) |

## Analysis

- [Liquidity dynamics: FIFO vs cancel-priority scheduling](liquidity_analysis.md) — compares how FIFO, cancel-only, and full ACE priority affect book depth, taker fill rates, and MM protection across a requote scenario.
- [Spoofing vulnerability analysis](spoofing.md) — documents how shred streaming enables a market maker to place phantom liquidity and reactively cancel before a taker fills, including selective layer cancellation that degrades taker entry prices without producing a detectable 0-fill outcome.

## Disclaimer

Unaudited proof of concept. Not production code.
