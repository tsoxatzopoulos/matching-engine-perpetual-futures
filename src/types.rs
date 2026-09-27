//! Core enums and identifiers.

use crate::fixed::{Price, Qty};

pub type OrderId = u64;
pub type AccountId = u64;
pub type SymbolId = u32;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    #[inline]
    pub fn opposite(self) -> Side {
        match self {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        }
    }

    #[inline]
    pub fn sign(self) -> i64 {
        match self {
            Side::Buy => 1,
            Side::Sell => -1,
        }
    }

    /// Signed quantity: positive for buys, negative for sells.
    #[inline]
    pub fn signed(self, q: Qty) -> Qty {
        Qty(q.0 * self.sign())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OrderType {
    Limit,
    Market,
    /// Breakout stop: buy fires when price rises to the trigger, sell when it falls.
    StopMarket,
    StopLimit,
    /// Take profit: buy fires when price falls to the trigger, sell when it rises.
    TakeProfitMarket,
    TakeProfitLimit,
    /// Stop that follows the price at a fixed distance (callback).
    TrailingStopMarket,
}

impl OrderType {
    #[inline]
    pub fn is_conditional(self) -> bool {
        !matches!(self, OrderType::Limit | OrderType::Market)
    }

    /// Whether the order executes as a market order (after triggering).
    #[inline]
    pub fn executes_as_market(self) -> bool {
        matches!(
            self,
            OrderType::Market
                | OrderType::StopMarket
                | OrderType::TakeProfitMarket
                | OrderType::TrailingStopMarket
        )
    }

    #[inline]
    pub fn has_limit_price(self) -> bool {
        !self.executes_as_market()
    }

    /// Price direction that fires a conditional order of this type on `side`.
    pub fn trigger_dir(self, side: Side) -> Option<TriggerDir> {
        use OrderType::*;
        Some(match (self, side) {
            (StopMarket | StopLimit | TrailingStopMarket, Side::Buy) => TriggerDir::Rising,
            (StopMarket | StopLimit | TrailingStopMarket, Side::Sell) => TriggerDir::Falling,
            (TakeProfitMarket | TakeProfitLimit, Side::Buy) => TriggerDir::Falling,
            (TakeProfitMarket | TakeProfitLimit, Side::Sell) => TriggerDir::Rising,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum TimeInForce {
    /// Good till cancel.
    #[default]
    Gtc,
    /// Immediate or cancel: fill what's possible, cancel the rest.
    Ioc,
    /// Fill or kill: fill completely immediately or reject.
    Fok,
    /// Good till crossing (post-only): rejected if it would take liquidity.
    Gtx,
}

/// Which price feeds a conditional order's trigger.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum TriggerBy {
    #[default]
    LastPrice = 0,
    MarkPrice = 1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TriggerDir {
    /// Fires when price >= trigger.
    Rising,
    /// Fires when price <= trigger.
    Falling,
}

impl TriggerDir {
    #[inline]
    pub fn is_hit(self, trigger: Price, price: Price) -> bool {
        match self {
            TriggerDir::Rising => price >= trigger,
            TriggerDir::Falling => price <= trigger,
        }
    }
}

/// Self-trade prevention mode, applied by the taker.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum StpMode {
    /// Allow self trades.
    None,
    /// Cancel the incoming order's remainder.
    CancelTaker,
    /// Cancel the resting order and keep matching.
    #[default]
    CancelMaker,
    /// Cancel both.
    CancelBoth,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum MarginMode {
    #[default]
    Cross,
    Isolated,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OrderStatus {
    /// Conditional order waiting for its trigger.
    Untriggered,
    New,
    PartiallyFilled,
    Filled,
    Canceled,
    /// IOC/FOK/market remainder that could not be filled.
    Expired,
    Rejected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OrderOrigin {
    User,
    /// TP/SL created from the fills of a parent order.
    AttachedTpSl { parent: OrderId },
    /// Position-level TP/SL (closes the whole position).
    PositionTpSl,
    Liquidation,
}
