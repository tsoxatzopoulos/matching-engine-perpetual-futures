//! Order requests and live orders.

use crate::fixed::{Amount, Price, Qty, Rate, Round};
use crate::types::*;

/// Trailing-stop callback distance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrailingOffset {
    /// Fixed price distance.
    Absolute(Price),
    /// Distance as a fraction of the extreme price (0.01 = 1%).
    Rate(Rate),
}

impl TrailingOffset {
    pub fn distance(self, from: Price) -> Price {
        match self {
            TrailingOffset::Absolute(p) => p,
            TrailingOffset::Rate(r) => from.mul_rate(r, Round::Down),
        }
    }

    pub fn is_valid(self) -> bool {
        match self {
            TrailingOffset::Absolute(p) => p.is_pos(),
            TrailingOffset::Rate(r) => r.is_pos() && r < Rate::ONE,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TrailingSpec {
    pub offset: TrailingOffset,
    /// Tracking starts once price reaches this level (sell: >=, buy: <=).
    pub activation_price: Option<Price>,
}

/// Take-profit / stop-loss leg.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TpSl {
    pub trigger_price: Price,
    pub trigger_by: TriggerBy,
    /// `None` executes as market once triggered.
    pub limit_price: Option<Price>,
}

impl TpSl {
    pub fn market(trigger_price: Price) -> Self {
        Self { trigger_price, trigger_by: TriggerBy::LastPrice, limit_price: None }
    }

    pub fn limit(trigger_price: Price, limit_price: Price) -> Self {
        Self { trigger_price, trigger_by: TriggerBy::LastPrice, limit_price: Some(limit_price) }
    }

    pub fn by(mut self, trigger_by: TriggerBy) -> Self {
        self.trigger_by = trigger_by;
        self
    }
}

/// An order request as submitted by a client.
#[derive(Clone, Debug)]
pub struct NewOrder {
    pub account: AccountId,
    pub symbol: SymbolId,
    pub client_order_id: u64,
    pub side: Side,
    pub order_type: OrderType,
    pub qty: Qty,
    pub price: Option<Price>,
    pub tif: TimeInForce,
    pub reduce_only: bool,
    /// Conditional order that closes the whole position when triggered.
    pub close_position: bool,
    pub trigger_price: Option<Price>,
    pub trigger_by: TriggerBy,
    pub trailing: Option<TrailingSpec>,
    pub take_profit: Option<TpSl>,
    pub stop_loss: Option<TpSl>,
    /// Iceberg: quantity shown in the book at a time.
    pub display_qty: Option<Qty>,
    pub stp: StpMode,
}

impl NewOrder {
    fn base(account: AccountId, symbol: SymbolId, side: Side, order_type: OrderType, qty: Qty) -> Self {
        Self {
            account,
            symbol,
            client_order_id: 0,
            side,
            order_type,
            qty,
            price: None,
            tif: TimeInForce::Gtc,
            reduce_only: false,
            close_position: false,
            trigger_price: None,
            trigger_by: TriggerBy::LastPrice,
            trailing: None,
            take_profit: None,
            stop_loss: None,
            display_qty: None,
            stp: StpMode::default(),
        }
    }

    pub fn limit(account: AccountId, symbol: SymbolId, side: Side, price: Price, qty: Qty) -> Self {
        let mut o = Self::base(account, symbol, side, OrderType::Limit, qty);
        o.price = Some(price);
        o
    }

    pub fn market(account: AccountId, symbol: SymbolId, side: Side, qty: Qty) -> Self {
        let mut o = Self::base(account, symbol, side, OrderType::Market, qty);
        o.tif = TimeInForce::Ioc;
        o
    }

    pub fn stop_market(account: AccountId, symbol: SymbolId, side: Side, trigger: Price, qty: Qty) -> Self {
        let mut o = Self::base(account, symbol, side, OrderType::StopMarket, qty);
        o.trigger_price = Some(trigger);
        o
    }

    pub fn stop_limit(
        account: AccountId,
        symbol: SymbolId,
        side: Side,
        trigger: Price,
        price: Price,
        qty: Qty,
    ) -> Self {
        let mut o = Self::base(account, symbol, side, OrderType::StopLimit, qty);
        o.trigger_price = Some(trigger);
        o.price = Some(price);
        o
    }

    pub fn take_profit_market(account: AccountId, symbol: SymbolId, side: Side, trigger: Price, qty: Qty) -> Self {
        let mut o = Self::base(account, symbol, side, OrderType::TakeProfitMarket, qty);
        o.trigger_price = Some(trigger);
        o
    }

    pub fn take_profit_limit(
        account: AccountId,
        symbol: SymbolId,
        side: Side,
        trigger: Price,
        price: Price,
        qty: Qty,
    ) -> Self {
        let mut o = Self::base(account, symbol, side, OrderType::TakeProfitLimit, qty);
        o.trigger_price = Some(trigger);
        o.price = Some(price);
        o
    }

    pub fn trailing_stop(
        account: AccountId,
        symbol: SymbolId,
        side: Side,
        offset: TrailingOffset,
        activation_price: Option<Price>,
        qty: Qty,
    ) -> Self {
        let mut o = Self::base(account, symbol, side, OrderType::TrailingStopMarket, qty);
        o.trailing = Some(TrailingSpec { offset, activation_price });
        o
    }

    pub fn tif(mut self, tif: TimeInForce) -> Self {
        self.tif = tif;
        self
    }
    pub fn ioc(self) -> Self {
        self.tif(TimeInForce::Ioc)
    }
    pub fn fok(self) -> Self {
        self.tif(TimeInForce::Fok)
    }
    pub fn post_only(self) -> Self {
        self.tif(TimeInForce::Gtx)
    }
    pub fn reduce_only(mut self) -> Self {
        self.reduce_only = true;
        self
    }
    /// Close the entire position on trigger (quantity is ignored).
    pub fn close_position(mut self) -> Self {
        self.close_position = true;
        self.reduce_only = true;
        self.qty = Qty::ZERO;
        self
    }
    pub fn trigger_by(mut self, by: TriggerBy) -> Self {
        self.trigger_by = by;
        self
    }
    pub fn take_profit(mut self, tp: TpSl) -> Self {
        self.take_profit = Some(tp);
        self
    }
    pub fn stop_loss(mut self, sl: TpSl) -> Self {
        self.stop_loss = Some(sl);
        self
    }
    pub fn iceberg(mut self, display_qty: Qty) -> Self {
        self.display_qty = Some(display_qty);
        self
    }
    pub fn stp(mut self, mode: StpMode) -> Self {
        self.stp = mode;
        self
    }
    pub fn client_id(mut self, id: u64) -> Self {
        self.client_order_id = id;
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TrailingState {
    pub offset: TrailingOffset,
    pub activation_price: Option<Price>,
    pub activated: bool,
    /// Highest (sell) / lowest (buy) price seen since activation.
    pub extreme: Price,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TriggerState {
    /// Current trigger level (moves for trailing stops).
    pub price: Price,
    pub by: TriggerBy,
    pub dir: TriggerDir,
    pub trailing: Option<TrailingState>,
}

/// A live order: resting in the book or waiting in the conditional book.
#[derive(Clone, Debug)]
pub struct Order {
    pub id: OrderId,
    pub client_order_id: u64,
    pub account: AccountId,
    pub symbol: SymbolId,
    pub side: Side,
    pub order_type: OrderType,
    pub tif: TimeInForce,
    /// Limit price. For market-type orders this is the slippage-protection
    /// price, set when the order goes active.
    pub price: Price,
    pub qty: Qty,
    pub filled: Qty,
    /// Quantity currently shown in the book (smaller than leaves for icebergs).
    pub visible: Qty,
    pub display_qty: Option<Qty>,
    /// Sum of price*qty over fills, for the average fill price.
    pub cum_quote: Amount,
    pub reduce_only: bool,
    pub close_position: bool,
    pub trigger: Option<TriggerState>,
    pub take_profit: Option<TpSl>,
    pub stop_loss: Option<TpSl>,
    /// Live TP and SL children created from this order's fills.
    pub children: [Option<OrderId>; 2],
    pub stp: StpMode,
    pub status: OrderStatus,
    /// OCO partner: canceled when this order triggers.
    pub linked: Option<OrderId>,
    pub is_liquidation: bool,
    pub origin: OrderOrigin,
}

impl Order {
    pub fn blank(
        id: OrderId,
        account: AccountId,
        symbol: SymbolId,
        side: Side,
        order_type: OrderType,
        qty: Qty,
        price: Price,
    ) -> Self {
        Self {
            id,
            client_order_id: 0,
            account,
            symbol,
            side,
            order_type,
            tif: TimeInForce::Gtc,
            price,
            qty,
            filled: Qty::ZERO,
            visible: Qty::ZERO,
            display_qty: None,
            cum_quote: Amount::ZERO,
            reduce_only: false,
            close_position: false,
            trigger: None,
            take_profit: None,
            stop_loss: None,
            children: [None, None],
            stp: StpMode::default(),
            status: OrderStatus::New,
            linked: None,
            is_liquidation: false,
            origin: OrderOrigin::User,
        }
    }

    #[inline]
    pub fn leaves(&self) -> Qty {
        self.qty - self.filled
    }

    pub fn avg_price(&self) -> Price {
        if self.filled.is_zero() {
            Price::ZERO
        } else {
            self.cum_quote.div_qty(self.filled, Round::Down)
        }
    }

    #[inline]
    pub fn has_attachments(&self) -> bool {
        self.take_profit.is_some() || self.stop_loss.is_some()
    }
}

/// Stop level of a trailing order given the extreme price seen so far.
pub fn trailing_stop_price(side: Side, extreme: Price, offset: TrailingOffset) -> Price {
    let d = offset.distance(extreme);
    match side {
        Side::Sell => extreme - d,
        Side::Buy => extreme + d,
    }
}
