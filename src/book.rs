//! Limit order book with price-time priority.
//!
//! Resting orders are split in two parallel slabs indexed by the same slot:
//!
//! * `hot`: a 64-byte [`Resting`] node holding exactly what matching reads
//!   (price, quantities, account, side, flags) plus the intrusive
//!   doubly-linked list of its price level. One cache line per order.
//! * `cold`: the rest of the order ([`OrderMeta`]: TP/SL, trigger, client id,
//!   status...), touched only when an order is inserted, removed or inspected.
//!
//! Insert, cancel and fill are O(1) once the level is found. Levels are kept in
//! a `BTreeMap` because crypto prices span a very wide range, which rules out
//! the flat price-indexed array used by equity engines.

use std::collections::BTreeMap;

use crate::fixed::{Amount, Price, Qty};
use crate::hash::FastMap;
use crate::order::{Order, TpSl, TriggerState};
use crate::types::*;

const NIL: u32 = u32::MAX;

const REDUCE_ONLY: u8 = 1;
const ATTACHED: u8 = 2;
const LIVE: u8 = 4;

/// Hot part of a resting order: everything the matching loop touches.
#[repr(C, align(64))]
#[derive(Clone, Copy, Debug)]
pub struct Resting {
    pub id: OrderId,
    pub account: AccountId,
    pub price: Price,
    pub qty: Qty,
    pub filled: Qty,
    /// Quantity currently shown in the book (smaller than leaves for icebergs).
    pub visible: Qty,
    prev: u32,
    next: u32,
    pub side: Side,
    flags: u8,
}

const _: () = assert!(std::mem::size_of::<Resting>() == 64, "Resting must fill exactly one cache line");

impl Resting {
    #[inline]
    pub fn leaves(&self) -> Qty {
        self.qty - self.filled
    }

    #[inline]
    pub fn reduce_only(&self) -> bool {
        self.flags & REDUCE_ONLY != 0
    }

    /// Has attached TP/SL legs.
    #[inline]
    pub fn has_attachments(&self) -> bool {
        self.flags & ATTACHED != 0
    }

    #[inline]
    fn live(&self) -> bool {
        self.flags & LIVE != 0
    }
}

/// Cold part of a resting order.
#[derive(Clone, Debug)]
pub struct OrderMeta {
    pub client_order_id: u64,
    pub symbol: SymbolId,
    pub order_type: OrderType,
    pub tif: TimeInForce,
    pub display_qty: Option<Qty>,
    pub cum_quote: Amount,
    pub close_position: bool,
    pub trigger: Option<TriggerState>,
    pub take_profit: Option<TpSl>,
    pub stop_loss: Option<TpSl>,
    pub children: [Option<OrderId>; 2],
    pub stp: StpMode,
    pub status: OrderStatus,
    pub linked: Option<OrderId>,
    pub is_liquidation: bool,
    pub origin: OrderOrigin,
}

fn split(o: Order) -> (Resting, OrderMeta) {
    let mut flags = LIVE;
    if o.reduce_only {
        flags |= REDUCE_ONLY;
    }
    if o.has_attachments() {
        flags |= ATTACHED;
    }
    let hot = Resting {
        id: o.id,
        account: o.account,
        price: o.price,
        qty: o.qty,
        filled: o.filled,
        visible: o.visible,
        prev: NIL,
        next: NIL,
        side: o.side,
        flags,
    };
    let meta = OrderMeta {
        client_order_id: o.client_order_id,
        symbol: o.symbol,
        order_type: o.order_type,
        tif: o.tif,
        display_qty: o.display_qty,
        cum_quote: o.cum_quote,
        close_position: o.close_position,
        trigger: o.trigger,
        take_profit: o.take_profit,
        stop_loss: o.stop_loss,
        children: o.children,
        stp: o.stp,
        status: o.status,
        linked: o.linked,
        is_liquidation: o.is_liquidation,
        origin: o.origin,
    };
    (hot, meta)
}

fn assemble(h: &Resting, m: OrderMeta) -> Order {
    Order {
        id: h.id,
        client_order_id: m.client_order_id,
        account: h.account,
        symbol: m.symbol,
        side: h.side,
        order_type: m.order_type,
        tif: m.tif,
        price: h.price,
        qty: h.qty,
        filled: h.filled,
        visible: h.visible,
        display_qty: m.display_qty,
        cum_quote: m.cum_quote,
        reduce_only: h.reduce_only(),
        close_position: m.close_position,
        trigger: m.trigger,
        take_profit: m.take_profit,
        stop_loss: m.stop_loss,
        children: m.children,
        stp: m.stp,
        status: m.status,
        linked: m.linked,
        is_liquidation: m.is_liquidation,
        origin: m.origin,
    }
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
    hot: Vec<Resting>,
    cold: Vec<Option<OrderMeta>>,
    free: Vec<u32>,
    index: FastMap<OrderId, u32>,
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
    fn slot(&self, id: OrderId) -> Option<u32> {
        self.index.get(&id).copied()
    }

    /// Hot view of a resting order.
    #[inline]
    pub fn resting(&self, id: OrderId) -> Option<&Resting> {
        self.slot(id).map(|i| &self.hot[i as usize])
    }

    /// Cold view of a resting order.
    pub fn meta(&self, id: OrderId) -> Option<&OrderMeta> {
        self.slot(id).map(|i| self.cold[i as usize].as_ref().expect("live slot"))
    }

    /// Mutable cold view (status, children...).
    pub fn meta_mut(&mut self, id: OrderId) -> Option<&mut OrderMeta> {
        let i = self.slot(id)?;
        Some(self.cold[i as usize].as_mut().expect("live slot"))
    }

    /// Full copy of a resting order (assembled from both slabs).
    pub fn order(&self, id: OrderId) -> Option<Order> {
        let i = self.slot(id)? as usize;
        Some(assemble(&self.hot[i], self.cold[i].clone().expect("live slot")))
    }

    /// Appends the order at the back of its price level. `order.visible` must be set.
    pub fn insert(&mut self, order: Order) {
        debug_assert!(order.visible.is_pos() && order.visible <= order.leaves());
        let id = order.id;
        let (hot, meta) = split(order);
        let i = match self.free.pop() {
            Some(i) => {
                self.hot[i as usize] = hot;
                self.cold[i as usize] = Some(meta);
                i
            }
            None => {
                self.hot.push(hot);
                self.cold.push(Some(meta));
                (self.hot.len() - 1) as u32
            }
        };
        self.index.insert(id, i);
        self.link_back(i);
    }

    fn link_back(&mut self, i: u32) {
        let (side, price, visible, leaves) = {
            let o = &self.hot[i as usize];
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
            self.hot[tail as usize].next = i;
        }
        let n = &mut self.hot[i as usize];
        n.prev = tail;
        n.next = NIL;
    }

    fn unlink(&mut self, i: u32) {
        let (side, price, visible, leaves, prev, next) = {
            let n = &self.hot[i as usize];
            (n.side, n.price, n.visible, n.leaves(), n.prev, n.next)
        };
        if prev != NIL {
            self.hot[prev as usize].next = next;
        }
        if next != NIL {
            self.hot[next as usize].prev = prev;
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
        let n = &mut self.hot[i as usize];
        n.prev = NIL;
        n.next = NIL;
    }

    pub fn remove(&mut self, id: OrderId) -> Option<Order> {
        let i = self.index.remove(&id)?;
        self.unlink(i);
        let meta = self.cold[i as usize].take().expect("live slot");
        let hot = &mut self.hot[i as usize];
        let order = assemble(hot, meta);
        hot.flags = 0;
        self.free.push(i);
        Some(order)
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
    pub fn front(&self, side: Side, price: Price) -> Option<&Resting> {
        let levels = match side {
            Side::Buy => &self.bids,
            Side::Sell => &self.asks,
        };
        let level = levels.get(&price)?;
        (level.head != NIL).then(|| &self.hot[level.head as usize])
    }

    /// Applies a fill of `qty` against the visible part of a resting order.
    pub fn fill(&mut self, id: OrderId, qty: Qty, quote: Amount) -> &Resting {
        let i = self.index[&id] as usize;
        let (side, price) = {
            let o = &mut self.hot[i];
            debug_assert!(qty <= o.visible, "fill exceeds visible quantity");
            o.filled += qty;
            o.visible -= qty;
            (o.side, o.price)
        };
        self.cold[i].as_mut().expect("live slot").cum_quote += quote;
        let levels = match side {
            Side::Buy => &mut self.bids,
            Side::Sell => &mut self.asks,
        };
        let level = levels.get_mut(&price).expect("order level");
        level.visible -= qty;
        level.total -= qty;
        &self.hot[i]
    }

    /// Iceberg refresh: shows the next slice and moves the order to the back
    /// of its level (a refreshed slice loses time priority).
    pub fn replenish(&mut self, id: OrderId) {
        let i = self.index[&id];
        self.unlink(i);
        let display = self.cold[i as usize].as_ref().expect("live slot").display_qty;
        let o = &mut self.hot[i as usize];
        let leaves = o.leaves();
        o.visible = display.map_or(leaves, |d| d.min(leaves));
        self.link_back(i);
    }

    /// Lowers an order's total quantity in place, keeping time priority.
    pub fn reduce_qty(&mut self, id: OrderId, new_qty: Qty) {
        let i = self.index[&id] as usize;
        let (side, price, d_leaves, d_visible) = {
            let o = &mut self.hot[i];
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
                        let n = &self.hot[i as usize];
                        if n.account == acct {
                            if stop_at_own {
                                return false;
                            }
                        } else {
                            sum += n.leaves();
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

    /// Copies of all resting orders (slot order, not priority order).
    pub fn orders(&self) -> impl Iterator<Item = Order> + '_ {
        self.hot
            .iter()
            .zip(&self.cold)
            .filter(|(h, _)| h.live())
            .map(|(h, m)| assemble(h, m.clone().expect("live slot")))
    }
}
