#![allow(unexpected_cfgs)]

use std::ops::{Deref, DerefMut};

use apq_core::{AsyncIx, AsyncState, FromBytes, Program, SyncIx};

use pinocchio::{
    account_info::AccountInfo,
    entrypoint,
    program_error::ProgramError,
    pubkey::Pubkey,
    sysvars::{clock::Clock, Sysvar},
    ProgramResult,
};
use sokoban::{red_black_tree::RBNode, NodeAllocatorMap, SENTINEL};

pub mod async_instructions;
pub mod orderbook;
pub mod positions;
pub mod settlement;
pub mod state;
pub mod sync_instructions;

use async_instructions::{PerpsAsyncIx, PerpsAsyncIxArgs};
use state::{AsyncIxKey, PerpsAsyncIxType, PerpsState, QueuedInstruction};
use sync_instructions::PerpsSyncIx;

/// Args passed to queue_async: the user pubkey + instruction params
pub struct QueueAsyncArgs {
    pub user: [u8; 32],
    pub ix_type: PerpsAsyncIxType,
    pub price: u64,
    pub size: u64,
    pub side: u64,
    pub order_seq: u64,
    pub extra_user: [u8; 32],
}

impl PerpsState {
    pub fn peek_async(&self) -> Option<(u32, &RBNode<AsyncIxKey, QueuedInstruction>)> {
        let mut addr = self.async_queue.root;
        if addr == SENTINEL {
            return None;
        }
        let mut last_addr = addr;
        while addr != SENTINEL {
            last_addr = addr;
            addr = self.async_queue.get_left(addr);
        }
        Some((last_addr, self.async_queue.get_node(last_addr)))
    }

    pub fn pop_async(&mut self) -> Option<RBNode<AsyncIxKey, QueuedInstruction>> {
        let (_addr, &val) = self.peek_async()?;
        self.async_queue.remove(&val.key);
        Some(val)
    }

    #[cfg(test)]
    pub fn new_boxed() -> Box<Self> {
        // Allocate zeroed on heap directly to avoid stack overflow
        let layout = std::alloc::Layout::new::<PerpsState>();
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) as *mut PerpsState };
        let mut s = unsafe { Box::from_raw(ptr) };
        s.seq = 1;
        s.async_queue.initialize();
        s.orderbook.initialize();
        s
    }
}

impl FromBytes for PerpsState {
    type Target<'a> = &'a Self;
    type TargetMut<'a> = &'a mut Self;

    fn from_bytes<'a>(bytes: &'a [u8]) -> Result<&'a Self, ProgramError> {
        bytemuck::try_from_bytes(bytes).map_err(|_| ProgramError::InvalidAccountData)
    }

    fn from_bytes_mut<'a>(bytes: &'a mut [u8]) -> Result<&'a mut Self, ProgramError> {
        bytemuck::try_from_bytes_mut(bytes).map_err(|_| ProgramError::InvalidAccountData)
    }
}

impl AsyncState for PerpsState {
    type SyncIx = PerpsSyncIx;
    type AsyncIx = PerpsAsyncIx;
    type QueueArgs = QueueAsyncArgs;

    fn queue_async(
        &mut self,
        ixn: &Self::AsyncIx,
        args: &Self::QueueArgs,
    ) -> Result<(), ProgramError> {
        let slot = get_slot();
        let priority = *ixn as u64;
        let key = AsyncIxKey {
            slot,
            priority,
            seq: self.seq,
        };
        self.seq += 1;

        let queued = QueuedInstruction {
            user: args.user,
            ix_type: priority,
            price: args.price,
            size: args.size,
            side: args.side,
            order_seq: args.order_seq,
            extra_user: args.extra_user,
        };

        self.async_queue
            .insert(key, queued)
            .ok_or(ProgramError::Custom(0x20))?;

        pinocchio_log::log!(
            "Queued async ix priority={} seq={} slot={}",
            priority,
            key.seq,
            slot
        );
        Ok(())
    }

    fn process_next_async(&mut self) -> ProgramResult {
        if let Some(next) = self.pop_async() {
            let ix = PerpsAsyncIx::from_type(unsafe {
                core::mem::transmute::<u64, PerpsAsyncIxType>(next.value.ix_type)
            });
            let args = PerpsAsyncIxArgs {
                queued: next.value,
            };
            ix.process(&args, self)?;
        }
        Ok(())
    }

    fn has_pending_async(&self, slot: u64) -> bool {
        let Some((_addr, val)) = self.peek_async() else {
            return false;
        };
        val.key.slot + 1 <= slot
    }
}

fn get_slot() -> u64 {
    #[cfg(test)]
    {
        0
    }
    #[cfg(not(test))]
    {
        Clock::get().unwrap().slot
    }
}

pub struct PerpsProgram;

impl Program for PerpsProgram {
    type Sync = PerpsSyncIx;
    type Async = PerpsAsyncIx;
    type State = PerpsState;

    fn process(
        _program_id: &Pubkey,
        accounts: &[AccountInfo],
        instruction_data: &[u8],
    ) -> ProgramResult {
        let [state_account, user, _rem @ ..] = accounts else {
            return Err(ProgramError::NotEnoughAccountKeys);
        };

        let mut state_data = state_account.try_borrow_mut_data()?;

        // Initialize if seq == 0
        if unsafe { *state_data.as_ptr().cast::<u64>() == 0 } {
            initialize_state(&mut state_data);
        }
        let mut state = PerpsState::from_bytes_mut(&mut state_data[..])?;

        let ix_type = instruction_data[0];
        let ix_data = &instruction_data[1..];

        match ix_type {
            0 => {
                // Sync instruction
                pinocchio::msg!("Executing Synchronous Instruction");
                let sync_ix = <Self::Sync as FromBytes>::from_bytes(ix_data)?;
                sync_ix.process(ix_data, accounts, state.deref_mut())?;
            }
            1 => {
                // Queue async instruction
                // ix_data layout: [async_ix_type: u64, price: u64, size: u64, side: u64, order_seq: u64]
                // For liquidate, extra_user (liquidator) follows
                pinocchio::msg!("Queueing Asynchronous Instruction");

                let async_ix =
                    <Self::Async as FromBytes>::from_bytes(ix_data)?;

                let price = if ix_data.len() >= 16 {
                    unsafe { ix_data.as_ptr().add(8).cast::<u64>().read_unaligned() }
                } else {
                    0
                };
                let size = if ix_data.len() >= 24 {
                    unsafe { ix_data.as_ptr().add(16).cast::<u64>().read_unaligned() }
                } else {
                    0
                };
                let side = if ix_data.len() >= 32 {
                    unsafe { ix_data.as_ptr().add(24).cast::<u64>().read_unaligned() }
                } else {
                    0
                };
                let order_seq = if ix_data.len() >= 40 {
                    unsafe { ix_data.as_ptr().add(32).cast::<u64>().read_unaligned() }
                } else {
                    0
                };
                let extra_user = if ix_data.len() >= 72 {
                    let mut buf = [0u8; 32];
                    buf.copy_from_slice(&ix_data[40..72]);
                    buf
                } else {
                    [0u8; 32]
                };

                let ix_type_val = *async_ix.deref() as u64;
                let args = QueueAsyncArgs {
                    user: *user.key(),
                    ix_type: unsafe { core::mem::transmute::<u64, PerpsAsyncIxType>(ix_type_val) },
                    price,
                    size,
                    side,
                    order_seq,
                    extra_user,
                };
                state.queue_async(async_ix.deref(), &args)?;
            }
            2 => {
                // Crank: process pending async instructions
                pinocchio::msg!("Processing Asynchronous Instructions");
                let slot = get_slot();
                while state.has_pending_async(slot) {
                    state.process_next_async()?;
                }
                pinocchio_log::log!("Crank complete");
            }
            _ => return Err(ProgramError::InvalidInstructionData),
        }

        Ok(())
    }
}

fn initialize_state(state_data: &mut [u8]) {
    pinocchio_log::log!("Initializing perps state");
    let state: &mut PerpsState = bytemuck::from_bytes_mut(state_data);
    state.seq = 1;
    state.async_queue.initialize();
    state.orderbook.initialize();
}

entrypoint!(process_instruction);

pub fn process_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    instruction_data: &[u8],
) -> ProgramResult {
    PerpsProgram::process(program_id, accounts, instruction_data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::PerpsAsyncIxType;

    #[test]
    fn test_priority_ordering() {
        let mut state = PerpsState::new_boxed();

        // Queue: Take, Cancel, PlaceOrder, Liquidate — should process in priority order
        let queue_ix = |state: &mut PerpsState, ix_type: PerpsAsyncIxType| {
            let ix = PerpsAsyncIx::from_type(ix_type);
            let args = QueueAsyncArgs {
                user: [1u8; 32],
                ix_type,
                price: 100_000_000,
                size: 1,
                side: 0,
                order_seq: 0,
                extra_user: [2u8; 32],
            };
            state.queue_async(&ix, &args).unwrap();
        };

        // Queue in "wrong" order
        queue_ix(&mut state, PerpsAsyncIxType::Take);
        queue_ix(&mut state, PerpsAsyncIxType::Cancel);
        queue_ix(&mut state, PerpsAsyncIxType::PlaceOrder);
        queue_ix(&mut state, PerpsAsyncIxType::Liquidate);

        assert_eq!(state.async_queue.len(), 4);

        // Pop should give: Liquidate(0), Cancel(1), PlaceOrder(2), Take(3)
        let expected = [0u64, 1, 2, 3];
        for &exp in &expected {
            let node = state.pop_async().unwrap();
            assert_eq!(
                node.key.priority, exp,
                "Expected priority {}, got {}",
                exp, node.key.priority
            );
        }
    }

    #[test]
    fn test_cancel_before_take_same_slot() {
        let mut state = PerpsState::new_boxed();

        // Both queued in same "slot" (seq 0 = slot 0 in test context)
        // Cancel has priority 1, Take has priority 3

        // Queue a take first (seq=1)
        let take_args = QueueAsyncArgs {
            user: [10u8; 32],
            ix_type: PerpsAsyncIxType::Take,
            price: 100_000_000,
            size: 1,
            side: 0,
            order_seq: 0,
            extra_user: [0u8; 32],
        };
        state
            .queue_async(&PerpsAsyncIx::Take, &take_args)
            .unwrap();

        // Queue a cancel second (seq=2) — but should still execute first
        let cancel_args = QueueAsyncArgs {
            user: [20u8; 32],
            ix_type: PerpsAsyncIxType::Cancel,
            price: 100_000_000,
            size: 0,
            side: 1,
            order_seq: 5,
            extra_user: [0u8; 32],
        };
        state
            .queue_async(&PerpsAsyncIx::Cancel, &cancel_args)
            .unwrap();

        // First pop should be Cancel (priority 1), not Take (priority 3)
        let first = state.pop_async().unwrap();
        assert_eq!(first.key.priority, 1, "Cancel should execute before Take");
        assert_eq!(first.value.user, [20u8; 32]);

        let second = state.pop_async().unwrap();
        assert_eq!(second.key.priority, 3, "Take should execute after Cancel");
        assert_eq!(second.value.user, [10u8; 32]);
    }

    #[test]
    fn test_fifo_within_same_priority() {
        let mut state = PerpsState::new_boxed();

        // Three cancels — should come out in seq order (FIFO)
        for i in 0u8..3 {
            let args = QueueAsyncArgs {
                user: [i + 1; 32],
                ix_type: PerpsAsyncIxType::Cancel,
                price: 0,
                size: 0,
                side: 0,
                order_seq: 0,
                extra_user: [0u8; 32],
            };
            state
                .queue_async(&PerpsAsyncIx::Cancel, &args)
                .unwrap();
        }

        for i in 0u8..3 {
            let node = state.pop_async().unwrap();
            assert_eq!(node.value.user, [i + 1; 32], "FIFO order violated");
        }
    }

    #[test]
    fn test_settlement_basic() {
        let mut state = PerpsState::new_boxed();

        let maker = [1u8; 32];
        let taker = [2u8; 32];

        // Give both users margin
        state.margins.credit(&maker, 100_000_000).unwrap();
        state.margins.credit(&taker, 100_000_000).unwrap();

        // Maker places an ask at price 150
        let price = 150u64;
        let size = 10u64;
        state.orderbook.place_ask(price, size, maker, 0);

        // Taker buys (side=0) at limit price 150
        let filled = settlement::execute_take(&mut state, &taker, price, size, 0);
        assert_eq!(filled, size, "Should fill entire order");

        // Taker should have a long position
        let taker_idx = state.positions.find(&taker).unwrap();
        let taker_pos = &state.positions.positions[taker_idx];
        assert_eq!(taker_pos.side, 0, "Taker should be long");
        assert_eq!(taker_pos.size, size);
        assert_eq!(taker_pos.entry_price, price);

        // Maker should have a short position
        let maker_idx = state.positions.find(&maker).unwrap();
        let maker_pos = &state.positions.positions[maker_idx];
        assert_eq!(maker_pos.side, 1, "Maker should be short");
        assert_eq!(maker_pos.size, size);
    }

    #[test]
    fn test_margin_deposit_withdraw() {
        let mut state = PerpsState::new_boxed();
        let user = [42u8; 32];

        state.margins.credit(&user, 1000).unwrap();
        assert_eq!(state.margins.get_balance(&user), 1000);

        state.margins.debit(&user, 300).unwrap();
        assert_eq!(state.margins.get_balance(&user), 700);

        // Overdraw should fail
        assert!(state.margins.debit(&user, 800).is_none());
        assert_eq!(state.margins.get_balance(&user), 700);
    }
}
