//! Conditional orders (stop, take profit, trailing stop) waiting for a trigger.
//!
//! Plain stops are indexed by trigger price in two ordered sets per price
//! source, so finding the orders a price move fires is a range scan. Trailing
//! stops move their trigger with the price and are updated on every tick.

use std::collections::{BTreeSet, HashMap};

use crate::fixed::Price;
use crate::order::{trailing_stop_price, Order};
use crate::types::{OrderId, Side, TriggerBy, TriggerDir};

#[derive(Default)]
pub struct ConditionalBook {
    orders: HashMap<OrderId, Order>,
    /// Fire when price >= trigger. Indexed by `TriggerBy`.
    rising: [BTreeSet<(Price, OrderId)>; 2],
    /// Fire when price <= trigger.
    falling: [BTreeSet<(Price, OrderId)>; 2],
    trailing: [BTreeSet<OrderId>; 2],
}

impl ConditionalBook {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.orders.len()
    }

    pub fn is_empty(&self) -> bool {
        self.orders.is_empty()
    }

    pub fn contains(&self, id: OrderId) -> bool {
        self.orders.contains_key(&id)
    }

    pub fn get(&self, id: OrderId) -> Option<&Order> {
        self.orders.get(&id)
    }

    /// Mutable access. Callers must not change the trigger (it is an index key).
    pub fn get_mut(&mut self, id: OrderId) -> Option<&mut Order> {
        self.orders.get_mut(&id)
    }

    pub fn orders(&self) -> impl Iterator<Item = &Order> + '_ {
        self.orders.values()
    }

    pub fn insert(&mut self, order: Order) {
        let t = order.trigger.expect("conditional order without trigger");
        let by = t.by as usize;
        if t.trailing.is_some() {
            self.trailing[by].insert(order.id);
        } else {
            match t.dir {
                TriggerDir::Rising => self.rising[by].insert((t.price, order.id)),
                TriggerDir::Falling => self.falling[by].insert((t.price, order.id)),
            };
        }
        self.orders.insert(order.id, order);
    }

    pub fn remove(&mut self, id: OrderId) -> Option<Order> {
        let order = self.orders.remove(&id)?;
        let t = order.trigger.expect("conditional order without trigger");
        let by = t.by as usize;
        if t.trailing.is_some() {
            self.trailing[by].remove(&id);
        } else {
            match t.dir {
                TriggerDir::Rising => self.rising[by].remove(&(t.price, id)),
                TriggerDir::Falling => self.falling[by].remove(&(t.price, id)),
            };
        }
        Some(order)
    }

    /// Appends the ids of orders fired by `price` on source `by` and advances
    /// trailing stops.
    pub fn collect_triggered(&mut self, by: TriggerBy, price: Price, out: &mut Vec<OrderId>) {
        if !price.is_pos() {
            return;
        }
        let b = by as usize;
        out.extend(self.rising[b].range(..=(price, OrderId::MAX)).map(|&(_, id)| id));
        out.extend(self.falling[b].range((price, 0)..).map(|&(_, id)| id));
        for &id in &self.trailing[b] {
            let order = self.orders.get_mut(&id).expect("trailing order");
            if update_trailing(order, price) {
                out.push(id);
            }
        }
    }
}

/// Advances a trailing stop with a new price; returns true if it fires.
fn update_trailing(order: &mut Order, price: Price) -> bool {
    let side = order.side;
    let t = order.trigger.as_mut().expect("trigger");
    let ts = t.trailing.as_mut().expect("trailing state");
    if !ts.activated {
        let reached = match (ts.activation_price, side) {
            (None, _) => true,
            (Some(a), Side::Sell) => price >= a,
            (Some(a), Side::Buy) => price <= a,
        };
        if !reached {
            return false;
        }
        ts.activated = true;
        ts.extreme = price;
    }
    match side {
        Side::Sell if price > ts.extreme => ts.extreme = price,
        Side::Buy if price < ts.extreme => ts.extreme = price,
        _ => {}
    }
    t.price = trailing_stop_price(side, ts.extreme, ts.offset);
    match side {
        Side::Sell => price <= t.price,
        Side::Buy => price >= t.price,
    }
}
