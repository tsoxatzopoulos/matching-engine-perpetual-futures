//! Accounts, positions and margin arithmetic.

use std::collections::{BTreeMap, BTreeSet};

use crate::fixed::{mul_div, notional, Amount, Price, Qty, Rate, Round};
use crate::market::{Market, SymbolSpec};
use crate::types::{AccountId, MarginMode, OrderId, Side, SymbolId};

/// Aggregate of an account's resting, non-reduce-only orders in one symbol.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OpenOrders {
    pub buy_qty: Qty,
    pub buy_notional: Amount,
    pub sell_qty: Qty,
    pub sell_notional: Amount,
}

impl OpenOrders {
    pub fn add(&mut self, side: Side, qty: Qty, price: Price) {
        let n = notional(price, qty);
        match side {
            Side::Buy => {
                self.buy_qty += qty;
                self.buy_notional += n;
            }
            Side::Sell => {
                self.sell_qty += qty;
                self.sell_notional += n;
            }
        }
    }

    pub fn sub(&mut self, side: Side, qty: Qty, price: Price) {
        let n = notional(price, qty);
        let (q, v) = match side {
            Side::Buy => (&mut self.buy_qty, &mut self.buy_notional),
            Side::Sell => (&mut self.sell_qty, &mut self.sell_notional),
        };
        *q -= qty;
        *v -= n;
        if !q.is_pos() {
            // Clear rounding dust once the side is empty.
            *q = Qty::ZERO;
            *v = Amount::ZERO;
        }
    }

    /// Initial margin reserved by open orders. Order quantity that would only
    /// reduce the current position is free, and since buys and sells can't
    /// both fully execute against the same position only the larger side counts.
    pub fn margin(&self, size: Qty, leverage: u32) -> Amount {
        let long = size.max(Qty::ZERO);
        let short = (-size).max(Qty::ZERO);
        let buys = increasing_notional(self.buy_qty, self.buy_notional, short);
        let sells = increasing_notional(self.sell_qty, self.sell_notional, long);
        buys.max(sells).div_leverage(leverage, Round::Up)
    }
}

fn increasing_notional(qty: Qty, notional: Amount, reducing: Qty) -> Amount {
    if qty <= reducing {
        Amount::ZERO
    } else {
        Amount(mul_div(notional.0, (qty - reducing).0, qty.0, Round::Up))
    }
}

/// One-way-mode position in one symbol.
#[derive(Clone, Debug)]
pub struct Position {
    /// Signed size: long > 0, short < 0.
    pub size: Qty,
    pub entry_price: Price,
    pub leverage: u32,
    pub margin_mode: MarginMode,
    /// Collateral locked in this position (isolated mode only).
    pub isolated_margin: Amount,
    pub realized_pnl: Amount,
    pub fees_paid: Amount,
    pub funding_paid: Amount,
    pub open: OpenOrders,
    /// All live orders of this account in this symbol (resting + conditional).
    pub order_ids: BTreeSet<OrderId>,
    /// Subset of `order_ids` that is reduce-only, checked on every position change.
    pub reduce_only_ids: BTreeSet<OrderId>,
    pub tp_order: Option<OrderId>,
    pub sl_order: Option<OrderId>,
}

impl Position {
    pub fn new(leverage: u32) -> Self {
        Self {
            size: Qty::ZERO,
            entry_price: Price::ZERO,
            leverage,
            margin_mode: MarginMode::Cross,
            isolated_margin: Amount::ZERO,
            realized_pnl: Amount::ZERO,
            fees_paid: Amount::ZERO,
            funding_paid: Amount::ZERO,
            open: OpenOrders::default(),
            order_ids: BTreeSet::new(),
            reduce_only_ids: BTreeSet::new(),
            tp_order: None,
            sl_order: None,
        }
    }

    pub fn track(&mut self, id: OrderId, reduce_only: bool) {
        self.order_ids.insert(id);
        if reduce_only {
            self.reduce_only_ids.insert(id);
        }
    }

    pub fn untrack(&mut self, id: OrderId) {
        self.order_ids.remove(&id);
        self.reduce_only_ids.remove(&id);
    }

    pub fn side(&self) -> Option<Side> {
        match self.size.signum() {
            1 => Some(Side::Buy),
            -1 => Some(Side::Sell),
            _ => None,
        }
    }

    pub fn notional(&self, mark: Price) -> Amount {
        notional(mark, self.size.abs())
    }

    pub fn unrealized_pnl(&self, mark: Price) -> Amount {
        notional(mark - self.entry_price, self.size)
    }

    pub fn initial_margin(&self, mark: Price) -> Amount {
        self.notional(mark).div_leverage(self.leverage, Round::Up)
    }

    pub fn maintenance_margin(&self, mark: Price, spec: &SymbolSpec) -> Amount {
        let n = self.notional(mark);
        let tier = spec.tier_or_last(n);
        (n.mul_rate(tier.mmr, Round::Up) - tier.maint_amount).max(Amount::ZERO)
    }

    pub fn order_margin(&self) -> Amount {
        self.open.margin(self.size, self.leverage)
    }

    /// Unrealized PnL relative to entry notional (ADL ranking input).
    pub fn pnl_ratio(&self, mark: Price) -> Rate {
        let entry_notional = notional(self.entry_price, self.size.abs());
        if !entry_notional.is_pos() {
            return Rate::ZERO;
        }
        Rate(mul_div(self.unrealized_pnl(mark).0, crate::fixed::SCALE, entry_notional.0, Round::Down))
    }
}

#[derive(Clone, Debug)]
pub struct Account {
    pub id: AccountId,
    /// Cross wallet balance (excludes collateral locked in isolated positions).
    pub balance: Amount,
    pub positions: BTreeMap<SymbolId, Position>,
    /// Per-account fee overrides (VIP tiers).
    pub maker_fee: Option<Rate>,
    pub taker_fee: Option<Rate>,
}

impl Account {
    pub fn new(id: AccountId) -> Self {
        Self { id, balance: Amount::ZERO, positions: BTreeMap::new(), maker_fee: None, taker_fee: None }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MarginSummary {
    pub wallet: Amount,
    pub cross_upnl: Amount,
    pub cross_initial_margin: Amount,
    pub cross_maintenance_margin: Amount,
    pub order_margin: Amount,
    pub isolated_margin: Amount,
    pub isolated_upnl: Amount,
    /// Cross equity: wallet + cross unrealized PnL.
    pub equity: Amount,
    /// Free to open orders or withdraw.
    pub available: Amount,
}

pub fn margin_summary(acct: &Account, markets: &[Market]) -> MarginSummary {
    let mut s = MarginSummary { wallet: acct.balance, ..Default::default() };
    for (&sym, pos) in &acct.positions {
        let m = &markets[sym as usize];
        s.order_margin += pos.order_margin();
        if pos.size.is_zero() {
            continue;
        }
        let mark = m.mark_price;
        match pos.margin_mode {
            MarginMode::Cross => {
                s.cross_upnl += pos.unrealized_pnl(mark);
                s.cross_initial_margin += pos.initial_margin(mark);
                s.cross_maintenance_margin += pos.maintenance_margin(mark, &m.spec);
            }
            MarginMode::Isolated => {
                s.isolated_margin += pos.isolated_margin;
                s.isolated_upnl += pos.unrealized_pnl(mark);
            }
        }
    }
    s.equity = s.wallet + s.cross_upnl;
    s.available = s.equity - s.cross_initial_margin - s.order_margin;
    s
}
