# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What This Is

ACE (Application Controlled Execution) — a Rust template for asynchronous Solana programs. Implements a pattern where sync instructions execute immediately while async instructions are queued with priority ordering and processed later.

## Build & Test

```bash
# Build the on-chain program (requires cargo-build-sbf / Solana toolchain)
cargo-build-sbf

# Run tests
cargo test --workspace

# Run the counter example (must build program first)
cd counter && cargo run --example counter
```

## Architecture

**Workspace crates:**

- `core` (`apq-core`) — Trait definitions for the async/sync program pattern. Uses `pinocchio` for low-level Solana account access.
- `counter` — Example implementation: a counter where decrements are prioritized over increments via a Red-Black Tree priority queue (`sokoban`).

**Core trait hierarchy (`core/src/lib.rs`):**

- `FromBytes` — Zero-copy deserialization (flexible between owned and borrowed)
- `SyncIx` — Synchronous instructions, executed immediately
- `AsyncIx` — Async instructions, must impl `Ord` for priority ordering
- `AsyncState` — State that manages both sync/async execution and the async queue
- `Program` — Top-level entrypoint tying `Sync`, `Async`, and `State` together

**Instruction routing (counter):** Byte 0 of instruction data selects the path:
- `0` → Sync instruction (parsed from remaining bytes)
- `1` → Queue async instruction (inserted into RBTree by slot + ix type + seq)
- `2` → Process pending async instructions (drains queue for past slots)

**Key dependencies:** `pinocchio` (account access), `bytemuck` (zero-copy Pod types), `sokoban` (RedBlackTree), `litesvm` (test runtime).
