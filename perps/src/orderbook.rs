use bytemuck::{Pod, Zeroable};
use sokoban::{NodeAllocatorMap, RedBlackTree, SENTINEL};

pub const MAX_ORDERS: usize = 1024;

#[derive(Copy, Clone, Default, PartialEq, Eq, Pod, Zeroable, Debug)]
#[repr(C)]
pub struct OrderId {
    /// Price in fixed-point (scaled by 1e6). For bids, stored inverted so RBTree
    /// min-traversal gives best bid (highest price). For asks, stored directly so
    /// min-traversal gives best ask (lowest price).
    pub price_key: u64,
    /// Sequence number for FIFO within same price level
    pub seq: u64,
}

impl PartialOrd for OrderId {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OrderId {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.price_key
            .cmp(&other.price_key)
            .then(self.seq.cmp(&other.seq))
    }
}

#[derive(Copy, Clone, Default, PartialEq, Eq, Pod, Zeroable, Debug)]
#[repr(C)]
pub struct Order {
    pub owner: [u8; 32],
    pub size: u64,
    pub price: u64,
    pub side: u64, // 0 = bid, 1 = ask
}

impl Order {
    pub fn is_bid(&self) -> bool {
        self.side == 0
    }
}

#[derive(Copy, Clone, Pod, Zeroable)]
#[repr(C)]
pub struct Orderbook {
    pub bids: RedBlackTree<OrderId, Order, MAX_ORDERS>,
    pub asks: RedBlackTree<OrderId, Order, MAX_ORDERS>,
}

impl Orderbook {
    pub fn initialize(&mut self) {
        self.bids.initialize();
        self.asks.initialize();
    }

    /// Insert a bid. Price key is inverted so tree min = best bid (highest price).
    pub fn place_bid(&mut self, price: u64, size: u64, owner: [u8; 32], seq: u64) -> bool {
        let key = OrderId {
            price_key: u64::MAX - price,
            seq,
        };
        let order = Order {
            owner,
            size,
            price,
            side: 0,
        };
        self.bids.insert(key, order).is_some()
    }

    /// Insert an ask. Price key is direct so tree min = best ask (lowest price).
    pub fn place_ask(&mut self, price: u64, size: u64, owner: [u8; 32], seq: u64) -> bool {
        let key = OrderId {
            price_key: price,
            seq,
        };
        let order = Order {
            owner,
            size,
            price,
            side: 1,
        };
        self.asks.insert(key, order).is_some()
    }

    /// Remove an order by side and order_id
    pub fn cancel_bid(&mut self, order_id: &OrderId) -> Option<Order> {
        let node = self.bids.get(order_id)?;
        let order = *node;
        self.bids.remove(order_id);
        Some(order)
    }

    pub fn cancel_ask(&mut self, order_id: &OrderId) -> Option<Order> {
        let node = self.asks.get(order_id)?;
        let order = *node;
        self.asks.remove(order_id);
        Some(order)
    }

    /// Peek at the best ask (lowest price)
    pub fn best_ask(&self) -> Option<(&OrderId, &Order)> {
        let mut addr = self.asks.root;
        if addr == SENTINEL {
            return None;
        }
        while self.asks.get_left(addr) != SENTINEL {
            addr = self.asks.get_left(addr);
        }
        let node = self.asks.get_node(addr);
        Some((&node.key, &node.value))
    }

    /// Peek at the best bid (highest price, but stored inverted)
    pub fn best_bid(&self) -> Option<(&OrderId, &Order)> {
        let mut addr = self.bids.root;
        if addr == SENTINEL {
            return None;
        }
        while self.bids.get_left(addr) != SENTINEL {
            addr = self.bids.get_left(addr);
        }
        let node = self.bids.get_node(addr);
        Some((&node.key, &node.value))
    }

    /// Remove the best ask from the book
    pub fn pop_best_ask(&mut self) -> Option<(OrderId, Order)> {
        let (key, _) = self.best_ask()?;
        let key = *key;
        let order = self.asks.get(&key).copied()?;
        self.asks.remove(&key);
        Some((key, order))
    }

    /// Remove the best bid from the book
    pub fn pop_best_bid(&mut self) -> Option<(OrderId, Order)> {
        let (key, _) = self.best_bid()?;
        let key = *key;
        let order = self.bids.get(&key).copied()?;
        self.bids.remove(&key);
        Some((key, order))
    }
}
