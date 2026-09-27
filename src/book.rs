//! Limit order book with price-time priority.
//!
//! Orders live in a slab (`Vec<Option<Node>>` + free list) and are chained in
//! an intrusive doubly-linked list per price level, so insert, cancel and
//! fill are O(1) once the level is found. Levels are kept in a `BTreeMap`
//! because crypto prices span a very wide range, which rules out the flat
//! price-indexed array used by equity engines.

use std::collections::{BTreeMap, HashMap};

use crate::fixed::{Amount, Price, Qty};
use crate::order::Order;
use crate::types::{AccountId, OrderId, Side};

const NIL: u32 = u32::MAX;

struct Node {
    order: Order,
    prev: u32,
    next: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct Level {
    head: u32,
    tail: u32,
    /// Quantity shown in the book.
    pub visible: Qty,
    /// Full remaining quantity including iceberg reserves.
    pub total: Qty,
    pub count: u32,
}

impl Level {
    fn empty() -> Self {
        Self { head: NIL, tail: NIL, visible: Qty::ZERO, total: Qty::ZERO, count: 0 }
    }
}

#[derive(Default)]
pub struct OrderBook {
    nodes: Vec<Option<Node>>,
    free: Vec<u32>,
    index: HashMap<OrderId, u32>,
    bids: BTreeMap<Price, Level>,
    asks: BTreeMap<Price, Level>,
}

impl OrderBook {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    pub fn contains(&self, id: OrderId) -> bool {
        self.index.contains_key(&id)
    }

    #[inline]
    fn node(&self, i: u32) -> &Node {
        self.nodes[i as usize].as_ref().expect("live node")
    }

    #[inline]
    fn node_mut(&mut self, i: u32) -> &mut Node {
        self.nodes[i as usize].as_mut().expect("live node")
    }

    pub fn get(&self, id: OrderId) -> Option<&Order> {
        self.index.get(&id).map(|&i| &self.node(i).order)
    }

    /// Mutable access for bookkeeping fields (status, children...). Callers
    /// must not change price, side, quantities or visibility this way.
    pub fn get_mut(&mut self, id: OrderId) -> Option<&mut Order> {
        let i = *self.index.get(&id)?;
        Some(&mut self.node_mut(i).order)
    }

    /// Appends the order at the back of its price level. `order.visible` must be set.
    pub fn insert(&mut self, order: Order) {
        debug_assert!(order.visible.is_pos() && order.visible <= order.leaves());
        let id = order.id;
        let node = Node { order, prev: NIL, next: NIL };
        let i = match self.free.pop() {
            Some(i) => {
                self.nodes[i as usize] = Some(node);
                i
            }
            None => {
                self.nodes.push(Some(node));
                (self.nodes.len() - 1) as u32
            }
        };
        self.index.insert(id, i);
        self.link_back(i);
    }

    fn link_back(&mut self, i: u32) {
        let (side, price, visible, leaves) = {
            let o = &self.node(i).order;
            (o.side, o.price, o.visible, o.leaves())
        };
        let levels = match side {
            Side::Buy => &mut self.bids,
            Side::Sell => &mut self.asks,
        };
        let level = levels.entry(price).or_insert_with(Level::empty);
        let tail = level.tail;
        if level.head == NIL {
            level.head = i;
        }
        level.tail = i;
        level.visible += visible;
        level.total += leaves;
        level.count += 1;
        if tail != NIL {
            self.node_mut(tail).next = i;
        }
        let n = self.node_mut(i);
        n.prev = tail;
        n.next = NIL;
    }

    fn unlink(&mut self, i: u32) {
        let (side, price, visible, leaves, prev, next) = {
            let n = self.node(i);
            (n.order.side, n.order.price, n.order.visible, n.order.leaves(), n.prev, n.next)
        };
        if prev != NIL {
            self.node_mut(prev).next = next;
        }
        if next != NIL {
            self.node_mut(next).prev = prev;
        }
        let levels = match side {
            Side::Buy => &mut self.bids,
            Side::Sell => &mut self.asks,
        };
        let level = levels.get_mut(&price).expect("order level");
        if level.head == i {
            level.head = next;
        }
        if level.tail == i {
            level.tail = prev;
        }
        level.visible -= visible;
        level.total -= leaves;
        level.count -= 1;
        if level.count == 0 {
            levels.remove(&price);
        }
        let n = self.node_mut(i);
        n.prev = NIL;
        n.next = NIL;
    }

    pub fn remove(&mut self, id: OrderId) -> Option<Order> {
        let i = self.index.remove(&id)?;
        self.unlink(i);
        let node = self.nodes[i as usize].take().expect("live node");
        self.free.push(i);
        Some(node.order)
    }

    /// Best price on the given side of the book (highest bid / lowest ask).
    #[inline]
    pub fn best_price(&self, side: Side) -> Option<Price> {
        match side {
            Side::Buy => self.bids.keys().next_back().copied(),
            Side::Sell => self.asks.keys().next().copied(),
        }
    }

    pub fn best_bid(&self) -> Option<Price> {
        self.best_price(Side::Buy)
    }

    pub fn best_ask(&self) -> Option<Price> {
        self.best_price(Side::Sell)
    }

    /// First order in time priority at `price`.
    #[inline]
    pub fn front(&self, side: Side, price: Price) -> Option<OrderId> {
        let levels = match side {
            Side::Buy => &self.bids,
            Side::Sell => &self.asks,
        };
        let level = levels.get(&price)?;
        (level.head != NIL).then(|| self.node(level.head).order.id)
    }

    /// Applies a fill of `qty` against the visible part of a resting order.
    pub fn fill(&mut self, id: OrderId, qty: Qty, quote: Amount) -> &Order {
        let i = self.index[&id];
        let (side, price) = {
            let o = &mut self.node_mut(i).order;
            debug_assert!(qty <= o.visible, "fill exceeds visible quantity");
            o.filled += qty;
            o.visible -= qty;
            o.cum_quote += quote;
            (o.side, o.price)
        };
        let levels = match side {
            Side::Buy => &mut self.bids,
            Side::Sell => &mut self.asks,
        };
        let level = levels.get_mut(&price).expect("order level");
        level.visible -= qty;
        level.total -= qty;
        &self.node(i).order
    }

    /// Iceberg refresh: shows the next slice and moves the order to the back
    /// of its level (a refreshed slice loses time priority).
    pub fn replenish(&mut self, id: OrderId) {
        let i = self.index[&id];
        self.unlink(i);
        {
            let o = &mut self.node_mut(i).order;
            let leaves = o.leaves();
            o.visible = o.display_qty.map_or(leaves, |d| d.min(leaves));
        }
        self.link_back(i);
    }

    /// Lowers an order's total quantity in place, keeping time priority.
    pub fn reduce_qty(&mut self, id: OrderId, new_qty: Qty) {
        let i = self.index[&id];
        let (side, price, d_leaves, d_visible) = {
            let o = &mut self.node_mut(i).order;
            debug_assert!(new_qty > o.filled && new_qty <= o.qty);
            let old_leaves = o.leaves();
            o.qty = new_qty;
            let new_leaves = o.leaves();
            let new_visible = o.visible.min(new_leaves);
            let d_visible = o.visible - new_visible;
            o.visible = new_visible;
            (o.side, o.price, old_leaves - new_leaves, d_visible)
        };
        let levels = match side {
            Side::Buy => &mut self.bids,
            Side::Sell => &mut self.asks,
        };
        let level = levels.get_mut(&price).expect("order level");
        level.total -= d_leaves;
        level.visible -= d_visible;
    }

    /// Top `n` levels of visible depth, best first.
    pub fn depth(&self, side: Side, n: usize) -> Vec<(Price, Qty)> {
        match side {
            Side::Buy => self.bids.iter().rev().take(n).map(|(p, l)| (*p, l.visible)).collect(),
            Side::Sell => self.asks.iter().take(n).map(|(p, l)| (*p, l.visible)).collect(),
        }
    }

    /// Quantity a taker on `taker_side` could fill up to `limit`, counting
    /// iceberg reserves. Orders of `exclude` are skipped, or end the scan if
    /// `stop_at_own` (taker-side self-trade prevention). Stops once `need` is reached.
    pub fn fillable(
        &self,
        taker_side: Side,
        limit: Price,
        exclude: Option<AccountId>,
        stop_at_own: bool,
        need: Qty,
    ) -> Qty {
        let mut sum = Qty::ZERO;
        let mut scan = |level: &Level| -> bool {
            match exclude {
                None => sum += level.total,
                Some(acct) => {
                    let mut i = level.head;
                    while i != NIL {
                        let n = self.node(i);
                        if n.order.account == acct {
                            if stop_at_own {
                                return false;
                            }
                        } else {
                            sum += n.order.leaves();
                        }
                        i = n.next;
                    }
                }
            }
            sum < need
        };
        match taker_side {
            Side::Buy => {
                for (p, l) in self.asks.iter() {
                    if *p > limit || !scan(l) {
                        break;
                    }
                }
            }
            Side::Sell => {
                for (p, l) in self.bids.iter().rev() {
                    if *p < limit || !scan(l) {
                        break;
                    }
                }
            }
        }
        sum
    }

    pub fn orders(&self) -> impl Iterator<Item = &Order> + '_ {
        self.nodes.iter().filter_map(|n| n.as_ref().map(|n| &n.order))
    }
}
