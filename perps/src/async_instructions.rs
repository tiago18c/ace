use apq_core::{
    deser_containers::OwnedOrBorrowed,
    AsyncIx, FromBytes,
};
use pinocchio::program_error::ProgramError;

use crate::orderbook::OrderId;
use crate::settlement;
use crate::state::{PerpsAsyncIxType, PerpsState, QueuedInstruction};

/// Async instruction enum — Ord determines priority.
/// Lower discriminant = higher priority = executes first in a slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u64)]
pub enum PerpsAsyncIx {
    Liquidate = 0,
    Cancel = 1,
    PlaceOrder = 2,
    Take = 3,
}

impl PerpsAsyncIx {
    const MAX_VARIANT: u64 = 3;

    pub fn from_type(t: PerpsAsyncIxType) -> Self {
        match t {
            PerpsAsyncIxType::Liquidate => PerpsAsyncIx::Liquidate,
            PerpsAsyncIxType::Cancel => PerpsAsyncIx::Cancel,
            PerpsAsyncIxType::PlaceOrder => PerpsAsyncIx::PlaceOrder,
            PerpsAsyncIxType::Take => PerpsAsyncIx::Take,
        }
    }
}

impl FromBytes for PerpsAsyncIx {
    type Target<'a> = OwnedOrBorrowed<'a, Self>;
    type TargetMut<'a> = apq_core::deser_containers::OwnedOrBorrowedMut<'a, Self>;

    fn from_bytes<'a>(bytes: &'a [u8]) -> Result<OwnedOrBorrowed<'a, Self>, ProgramError> {
        let (ix, _) = bytes
            .split_at_checked(8)
            .ok_or(ProgramError::InvalidInstructionData)?;
        let val = unsafe { ix.as_ptr().cast::<u64>().read_unaligned() };
        if val > Self::MAX_VARIANT {
            return Err(ProgramError::InvalidInstructionData);
        }
        Ok(OwnedOrBorrowed::Owned(unsafe {
            core::mem::transmute::<u64, PerpsAsyncIx>(val)
        }))
    }

    fn from_bytes_mut<'a>(
        _bytes: &'a mut [u8],
    ) -> Result<Self::TargetMut<'a>, ProgramError> {
        unimplemented!()
    }
}

/// Args passed when processing a queued async instruction
pub struct PerpsAsyncIxArgs {
    pub queued: QueuedInstruction,
}

impl AsyncIx for PerpsAsyncIx {
    type Args = PerpsAsyncIxArgs;

    fn process<S: apq_core::AsyncState>(
        &self,
        args: &Self::Args,
        state: &mut S,
    ) -> pinocchio::ProgramResult {
        let perps = unsafe { &mut *(state as *mut S as *mut PerpsState) };
        let q = &args.queued;

        match self {
            PerpsAsyncIx::Liquidate => {
                let success = settlement::execute_liquidation(
                    perps,
                    &q.user,        // target
                    &q.extra_user,  // liquidator
                );
                if success {
                    pinocchio_log::log!("Liquidation executed");
                } else {
                    pinocchio_log::log!("Liquidation skipped (not liquidatable)");
                }
                Ok(())
            }
            PerpsAsyncIx::Cancel => {
                let side = q.side;
                let order_id = if side == 0 {
                    // Bid: price was stored inverted
                    OrderId {
                        price_key: u64::MAX - q.price,
                        seq: q.order_seq,
                    }
                } else {
                    OrderId {
                        price_key: q.price,
                        seq: q.order_seq,
                    }
                };

                let cancelled = if side == 0 {
                    perps.orderbook.cancel_bid(&order_id)
                } else {
                    perps.orderbook.cancel_ask(&order_id)
                };

                if let Some(order) = cancelled {
                    // Return reserved margin
                    let notional = order.price.saturating_mul(order.size);
                    let margin_reserved = notional / 10;
                    perps.margins.credit(&q.user, margin_reserved);
                    pinocchio_log::log!("Order cancelled, margin returned");
                } else {
                    pinocchio_log::log!("Cancel: order not found (may already be filled)");
                }
                Ok(())
            }
            PerpsAsyncIx::PlaceOrder => {
                let price = q.price;
                let size = q.size;
                let side = q.side;

                // Reserve margin: 10% of notional
                let notional = price.saturating_mul(size);
                let margin_required = notional / 10;

                if perps.margins.debit(&q.user, margin_required).is_none() {
                    pinocchio_log::log!("PlaceOrder failed: insufficient margin");
                    return Ok(());
                }

                let placed = if side == 0 {
                    perps
                        .orderbook
                        .place_bid(price, size, q.user, q.order_seq)
                } else {
                    perps
                        .orderbook
                        .place_ask(price, size, q.user, q.order_seq)
                };

                if placed {
                    pinocchio_log::log!("Order placed: price={} size={} side={}", price, size, side);
                } else {
                    // Refund margin if placement failed
                    perps.margins.credit(&q.user, margin_required);
                    pinocchio_log::log!("PlaceOrder failed: book full");
                }
                Ok(())
            }
            PerpsAsyncIx::Take => {
                let filled = settlement::execute_take(
                    perps,
                    &q.user,
                    q.price,
                    q.size,
                    q.side,
                );
                pinocchio_log::log!("Take executed: filled {}", filled);
                Ok(())
            }
        }
    }
}
