use bytemuck::{Pod, Zeroable};
use sokoban::RedBlackTree;

use crate::orderbook::Orderbook;
use crate::positions::{MarginMap, PositionMap};

pub const MAX_QUEUE: usize = 4096;

/// Maintenance margin: 5% (500 bps)
pub const MAINTENANCE_MARGIN_BPS: u64 = 500;

/// Price scale factor (prices are price * PRICE_SCALE)
pub const PRICE_SCALE: u64 = 1_000_000;

/// Sort key for the async queue: (slot, priority, seq)
/// Lower priority value = higher execution priority.
#[derive(Copy, Clone, Zeroable, Pod, PartialEq, Eq, PartialOrd, Ord, Default, Debug)]
#[repr(C)]
pub struct AsyncIxKey {
    pub slot: u64,
    pub priority: u64,
    pub seq: u64,
}

/// What kind of async instruction is queued
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u64)]
pub enum PerpsAsyncIxType {
    Liquidate = 0,
    Cancel = 1,
    PlaceOrder = 2,
    Take = 3,
}

/// Params stored in the queue for each instruction.
/// Union-style: different fields used by different ix types.
#[derive(Copy, Clone, Default, PartialEq, Eq, Pod, Zeroable, Debug)]
#[repr(C)]
pub struct QueuedInstruction {
    pub user: [u8; 32],
    pub ix_type: u64,
    // Cancel: price_key + seq identify the order to cancel
    // PlaceOrder: price = limit price, size = order size, side = 0 bid / 1 ask
    // Take: price = limit price, size = max size, side = 0 buy / 1 sell
    // Liquidate: target_user is stored in `user`, liquidator in `extra_user`
    pub price: u64,
    pub size: u64,
    pub side: u64,
    pub order_seq: u64,     // for cancel: the seq of the order to cancel
    pub extra_user: [u8; 32], // for liquidate: the liquidator's pubkey
}

/// Top-level program state. Everything lives in one account (demo simplicity).
#[derive(Copy, Clone, Pod, Zeroable)]
#[repr(C)]
pub struct PerpsState {
    pub seq: u64,
    pub oracle_price: u64,
    pub funding_rate: i64,
    pub last_funding_slot: u64,

    pub margins: MarginMap,
    pub positions: PositionMap,
    pub orderbook: Orderbook,
    pub async_queue: RedBlackTree<AsyncIxKey, QueuedInstruction, MAX_QUEUE>,
}
