use bytemuck::{Pod, Zeroable};

pub const MAX_POSITIONS: usize = 256;

/// Side: 0 = long, 1 = short
#[derive(Copy, Clone, Default, PartialEq, Eq, Pod, Zeroable, Debug)]
#[repr(C)]
pub struct Position {
    pub owner: [u8; 32],
    pub size: u64,
    pub entry_price: u64,
    pub side: u64,       // 0 = long, 1 = short
    pub is_active: u64,  // 0 = empty slot, 1 = active
}

impl Position {
    pub fn is_long(&self) -> bool {
        self.side == 0
    }

    /// Unrealized PnL in price units (scaled same as price).
    /// Returns (pnl, is_positive).
    pub fn unrealized_pnl(&self, oracle_price: u64) -> (u64, bool) {
        if self.is_long() {
            if oracle_price >= self.entry_price {
                let pnl = (oracle_price - self.entry_price)
                    .checked_mul(self.size)
                    .unwrap_or(0);
                (pnl, true)
            } else {
                let pnl = (self.entry_price - oracle_price)
                    .checked_mul(self.size)
                    .unwrap_or(0);
                (pnl, false)
            }
        } else {
            // short
            if oracle_price <= self.entry_price {
                let pnl = (self.entry_price - oracle_price)
                    .checked_mul(self.size)
                    .unwrap_or(0);
                (pnl, true)
            } else {
                let pnl = (oracle_price - self.entry_price)
                    .checked_mul(self.size)
                    .unwrap_or(0);
                (pnl, false)
            }
        }
    }

    /// Check if position is undercollateralized.
    /// margin is the user's available collateral (in price units * PRICE_SCALE).
    /// maintenance_margin_bps: e.g. 500 = 5%
    pub fn is_liquidatable(&self, oracle_price: u64, margin: u64, maintenance_margin_bps: u64) -> bool {
        if self.is_active == 0 || self.size == 0 {
            return false;
        }
        let (pnl, is_positive) = self.unrealized_pnl(oracle_price);
        let effective_margin = if is_positive {
            margin.saturating_add(pnl)
        } else {
            margin.saturating_sub(pnl)
        };
        // Required maintenance margin = notional * maintenance_margin_bps / 10000
        let notional = oracle_price.saturating_mul(self.size);
        let required = notional / 10000 * maintenance_margin_bps;
        effective_margin < required
    }
}

/// Fixed-size array of positions. Simple linear scan — fine for demo scale.
#[derive(Copy, Clone, Pod, Zeroable)]
#[repr(C)]
pub struct PositionMap {
    pub positions: [Position; MAX_POSITIONS],
}

impl PositionMap {
    /// Find position by owner. Returns index.
    pub fn find(&self, owner: &[u8; 32]) -> Option<usize> {
        self.positions.iter().position(|p| p.is_active == 1 && p.owner == *owner)
    }

    /// Find or allocate a slot for owner. Returns index.
    pub fn find_or_alloc(&mut self, owner: &[u8; 32]) -> Option<usize> {
        if let Some(idx) = self.find(owner) {
            return Some(idx);
        }
        // Find empty slot
        let idx = self.positions.iter().position(|p| p.is_active == 0)?;
        self.positions[idx].owner = *owner;
        self.positions[idx].is_active = 1;
        Some(idx)
    }

    /// Close a position (mark slot as empty)
    pub fn close(&mut self, idx: usize) {
        self.positions[idx] = Position::default();
    }
}

/// Per-user margin balance. Simple fixed-size map.
pub const MAX_USERS: usize = 256;

#[derive(Copy, Clone, Default, PartialEq, Eq, Pod, Zeroable, Debug)]
#[repr(C)]
pub struct UserMargin {
    pub owner: [u8; 32],
    pub balance: u64,
    pub is_active: u64,
}

#[derive(Copy, Clone, Pod, Zeroable)]
#[repr(C)]
pub struct MarginMap {
    pub users: [UserMargin; MAX_USERS],
}

impl MarginMap {
    pub fn find(&self, owner: &[u8; 32]) -> Option<usize> {
        self.users.iter().position(|u| u.is_active == 1 && u.owner == *owner)
    }

    pub fn find_or_alloc(&mut self, owner: &[u8; 32]) -> Option<usize> {
        if let Some(idx) = self.find(owner) {
            return Some(idx);
        }
        let idx = self.users.iter().position(|u| u.is_active == 0)?;
        self.users[idx].owner = *owner;
        self.users[idx].is_active = 1;
        Some(idx)
    }

    pub fn get_balance(&self, owner: &[u8; 32]) -> u64 {
        self.find(owner)
            .map(|idx| self.users[idx].balance)
            .unwrap_or(0)
    }

    pub fn credit(&mut self, owner: &[u8; 32], amount: u64) -> Option<()> {
        let idx = self.find_or_alloc(owner)?;
        self.users[idx].balance = self.users[idx].balance.checked_add(amount)?;
        Some(())
    }

    pub fn debit(&mut self, owner: &[u8; 32], amount: u64) -> Option<()> {
        let idx = self.find(owner)?;
        if self.users[idx].balance < amount {
            return None;
        }
        self.users[idx].balance -= amount;
        Some(())
    }
}
