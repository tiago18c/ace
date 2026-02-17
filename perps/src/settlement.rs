use sokoban::NodeAllocatorMap;

use crate::positions::Position;
use crate::state::{PerpsState, MAINTENANCE_MARGIN_BPS};

/// Execute a take (market order) against the book.
/// side: 0 = buy (match against asks), 1 = sell (match against bids)
/// Returns number of units filled.
pub fn execute_take(
    state: &mut PerpsState,
    taker: &[u8; 32],
    limit_price: u64,
    mut size: u64,
    side: u64,
) -> u64 {
    let mut total_filled = 0u64;

    if side == 0 {
        // Buy: match against asks (ascending price)
        while size > 0 {
            let Some((best_key, best_order)) = state.orderbook.best_ask() else {
                break;
            };
            if best_order.price > limit_price {
                break;
            }
            let fill_price = best_order.price;
            let fill_size = size.min(best_order.size);
            let maker = best_order.owner;
            let best_key = *best_key;

            if fill_size == best_order.size {
                state.orderbook.asks.remove(&best_key);
            } else {
                if let Some(order) = state.orderbook.asks.get_mut(&best_key) {
                    order.size -= fill_size;
                }
            }

            settle_fill(state, &maker, taker, fill_price, fill_size, side);
            size -= fill_size;
            total_filled += fill_size;
        }
    } else {
        // Sell: match against bids (descending price, stored inverted)
        while size > 0 {
            let Some((best_key, best_order)) = state.orderbook.best_bid() else {
                break;
            };
            if best_order.price < limit_price {
                break;
            }
            let fill_price = best_order.price;
            let fill_size = size.min(best_order.size);
            let maker = best_order.owner;
            let best_key = *best_key;

            if fill_size == best_order.size {
                state.orderbook.bids.remove(&best_key);
            } else {
                if let Some(order) = state.orderbook.bids.get_mut(&best_key) {
                    order.size -= fill_size;
                }
            }

            settle_fill(state, &maker, taker, fill_price, fill_size, side);
            size -= fill_size;
            total_filled += fill_size;
        }
    }

    total_filled
}

/// Settle a single fill: update positions for maker and taker.
/// taker_side: 0 = taker buying (long), 1 = taker selling (short)
fn settle_fill(
    state: &mut PerpsState,
    maker: &[u8; 32],
    taker: &[u8; 32],
    fill_price: u64,
    fill_size: u64,
    taker_side: u64,
) {
    let notional = fill_price.saturating_mul(fill_size);
    let margin_required = notional / 10; // 10% initial margin

    if state.margins.debit(taker, margin_required).is_none() {
        return;
    }

    // Update taker position
    update_position(state, taker, fill_price, fill_size, taker_side);

    // Maker gets the opposite side
    let maker_side = if taker_side == 0 { 1 } else { 0 };
    update_position(state, maker, fill_price, fill_size, maker_side);

    // Free up maker's reserved margin (was locked at place time)
    state.margins.credit(maker, margin_required);
}

/// Update a user's position after a fill. Handles new, add, reduce, and flip.
fn update_position(
    state: &mut PerpsState,
    user: &[u8; 32],
    fill_price: u64,
    fill_size: u64,
    fill_side: u64,
) {
    let Some(idx) = state.positions.find_or_alloc(user) else {
        return;
    };

    let pos = state.positions.positions[idx];
    if pos.size == 0 {
        // New position
        let p = &mut state.positions.positions[idx];
        p.entry_price = fill_price;
        p.size = fill_size;
        p.side = fill_side;
    } else if pos.side == fill_side {
        // Adding to position — weighted average entry
        let total_notional = pos.entry_price * pos.size + fill_price * fill_size;
        let new_size = pos.size + fill_size;
        let p = &mut state.positions.positions[idx];
        p.size = new_size;
        p.entry_price = total_notional / new_size;
    } else {
        // Reducing/flipping position
        if fill_size >= pos.size {
            let close_size = pos.size;
            realize_pnl(state, user, pos.entry_price, fill_price, close_size, pos.side);
            let remaining = fill_size - close_size;
            if remaining > 0 {
                let p = &mut state.positions.positions[idx];
                p.entry_price = fill_price;
                p.size = remaining;
                p.side = fill_side;
            } else {
                state.positions.close(idx);
            }
        } else {
            realize_pnl(state, user, pos.entry_price, fill_price, fill_size, pos.side);
            state.positions.positions[idx].size -= fill_size;
        }
    }
}

/// Realize PnL and credit/debit margin
fn realize_pnl(
    state: &mut PerpsState,
    user: &[u8; 32],
    entry_price: u64,
    exit_price: u64,
    size: u64,
    side: u64,
) {
    let pos = Position {
        owner: *user,
        size,
        entry_price,
        side,
        is_active: 1,
    };
    let (pnl, is_positive) = pos.unrealized_pnl(exit_price);
    if is_positive {
        state.margins.credit(user, pnl);
    } else {
        let _ = state.margins.debit(user, pnl);
    }
}

/// Execute a liquidation
pub fn execute_liquidation(
    state: &mut PerpsState,
    target: &[u8; 32],
    liquidator: &[u8; 32],
) -> bool {
    let Some(idx) = state.positions.find(target) else {
        return false;
    };
    let pos = state.positions.positions[idx];
    let margin = state.margins.get_balance(target);

    if !pos.is_liquidatable(state.oracle_price, margin, MAINTENANCE_MARGIN_BPS) {
        return false;
    }

    let oracle_price = state.oracle_price;
    realize_pnl(state, target, pos.entry_price, oracle_price, pos.size, pos.side);
    state.positions.close(idx);

    let remaining = state.margins.get_balance(target);
    let reward = remaining / 2;
    if reward > 0 {
        let _ = state.margins.debit(target, reward);
        let _ = state.margins.credit(liquidator, reward);
    }

    true
}
