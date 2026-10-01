//! The matching engine: a single-threaded, deterministic state machine.
//!
//! Commands go in, events come out. All symbols share one engine instance
//! because cross margin couples every position of an account. Given the same
//! command sequence the engine always produces the same events, so a command
//! journal is enough to rebuild state or run a hot standby.

use std::collections::BTreeSet;

use crate::account::{margin_summary, Account, MarginSummary, Position};
use crate::events::{BalanceReason, DoneReason, Event, RejectReason};
use crate::hash::{FastMap, FastSet};
use crate::fixed::{mul_div, notional, Amount, Price, Qty, Rate, Round, SCALE};
use crate::market::{Market, SymbolSpec};
use crate::order::{trailing_stop_price, NewOrder, Order, TpSl, TrailingState, TriggerState};
use crate::risk::{compute_bands, AdlRanking, Armed};
use crate::types::*;

/// Upper bound on trigger/liquidation cascade steps per command.
const MAX_SETTLE_STEPS: usize = 100_000;
/// Upper bound on positions liquidated in one cross-margin liquidation.
const MAX_CROSS_LIQUIDATIONS: usize = 64;

#[derive(Clone, Debug)]
pub enum Command {
    AddMarket { spec: SymbolSpec, price: Price },
    Deposit { account: AccountId, amount: Amount },
    Withdraw { account: AccountId, amount: Amount },
    FundInsurance { amount: Amount },
    SetFeeRates { account: AccountId, maker: Option<Rate>, taker: Option<Rate> },
    PlaceOrder(NewOrder),
    CancelOrder { account: AccountId, symbol: SymbolId, order_id: OrderId },
    CancelAll { account: AccountId, symbol: Option<SymbolId> },
    AmendOrder {
        account: AccountId,
        symbol: SymbolId,
        order_id: OrderId,
        price: Option<Price>,
        qty: Option<Qty>,
        trigger_price: Option<Price>,
    },
    SetLeverage { account: AccountId, symbol: SymbolId, leverage: u32 },
    SetMarginMode { account: AccountId, symbol: SymbolId, mode: MarginMode },
    AdjustIsolatedMargin { account: AccountId, symbol: SymbolId, delta: Amount },
    SetPositionTpSl { account: AccountId, symbol: SymbolId, take_profit: Option<TpSl>, stop_loss: Option<TpSl> },
    MarkPrice { symbol: SymbolId, mark: Price, index: Price },
    Funding { symbol: SymbolId, rate: Rate },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Depth {
    pub bids: Vec<(Price, Qty)>,
    pub asks: Vec<(Price, Qty)>,
}

/// State of a running liquidation pass (`check_liquidations`).
struct LiqPass {
    symbol: SymbolId,
    /// Accounts touched by a fill during the pass.
    seen: FastSet<AccountId>,
    /// First touches: (account, was a holder of `symbol` when the pass started).
    touched: Vec<(AccountId, bool)>,
}

/// How a taker's matching loop ended.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MatchEnd {
    Done,
    SelfTrade,
    ReduceOnly,
}

pub struct Engine {
    markets: Vec<Market>,
    accounts: FastMap<AccountId, Account>,
    insurance_fund: Amount,
    next_order_id: OrderId,
    next_trade_id: u64,
    events: Vec<Event>,
    /// Symbols whose prices moved and need a trigger scan.
    dirty: Vec<SymbolId>,
    /// Positions that changed and need reduce-only order maintenance.
    touched: Vec<(AccountId, SymbolId)>,
    /// Spare buffer swapped with `touched` in `settle()` to keep its capacity.
    touched_spare: Vec<(AccountId, SymbolId)>,
    /// Accounts whose risk bands are stale (see `risk`).
    risk_dirty: Vec<AccountId>,
    liq_pass: Option<LiqPass>,
    /// Per-symbol ADL ranking, built lazily during a liquidation pass.
    adl_rankings: Vec<Option<AdlRanking>>,
    armed_buf: Armed,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

#[inline]
fn crosses(taker_side: Side, limit: Price, opposite_best: Price) -> bool {
    match taker_side {
        Side::Buy => opposite_best <= limit,
        Side::Sell => opposite_best >= limit,
    }
}

#[cold]
#[inline(never)]
fn update_ranking(ranking: &mut AdlRanking, account: AccountId, pos: &Position, mark: Price) {
    ranking.update(account, pos, mark);
}

/// Price at which a position's equity reaches zero:
/// `margin + (P - entry) * size = 0`, rounded to a tick toward the entry.
fn bankruptcy_price(entry: Price, size: Qty, margin: Amount, tick: Price) -> Price {
    let delta = Price(mul_div(margin.0, SCALE, size.0, Round::Down));
    let p = entry - delta;
    let p = if size.is_pos() { p.round_to(tick, Round::Up) } else { p.round_to(tick, Round::Down) };
    p.max(tick)
}

impl Engine {
    pub fn new() -> Self {
        Self {
            markets: Vec::new(),
            accounts: FastMap::default(),
            insurance_fund: Amount::ZERO,
            next_order_id: 1,
            next_trade_id: 1,
            events: Vec::new(),
            dirty: Vec::new(),
            touched: Vec::new(),
            touched_spare: Vec::new(),
            risk_dirty: Vec::new(),
            liq_pass: None,
            adl_rankings: Vec::new(),
            armed_buf: Armed::default(),
        }
    }

    // ------------------------------------------------------------------
    // Queries
    // ------------------------------------------------------------------

    pub fn market(&self, symbol: SymbolId) -> Option<&Market> {
        self.markets.get(symbol as usize)
    }

    pub fn markets(&self) -> &[Market] {
        &self.markets
    }

    pub fn account(&self, account: AccountId) -> Option<&Account> {
        self.accounts.get(&account)
    }

    pub fn position(&self, account: AccountId, symbol: SymbolId) -> Option<&Position> {
        self.accounts.get(&account)?.positions.get(&symbol)
    }

    pub fn margin(&self, account: AccountId) -> Option<MarginSummary> {
        self.accounts.get(&account).map(|a| margin_summary(a, &self.markets))
    }

    /// Copy of a live order (resting or conditional).
    pub fn order(&self, symbol: SymbolId, id: OrderId) -> Option<Order> {
        let m = self.market(symbol)?;
        m.book.order(id).or_else(|| m.cond.get(id).cloned())
    }

    pub fn open_orders(&self, account: AccountId, symbol: SymbolId) -> Vec<Order> {
        let Some(pos) = self.position(account, symbol) else { return Vec::new() };
        pos.order_ids.iter().filter_map(|&id| self.order(symbol, id)).collect()
    }

    pub fn depth(&self, symbol: SymbolId, levels: usize) -> Option<Depth> {
        let m = self.market(symbol)?;
        Some(Depth { bids: m.book.depth(Side::Buy, levels), asks: m.book.depth(Side::Sell, levels) })
    }

    pub fn insurance_fund(&self) -> Amount {
        self.insurance_fund
    }

    /// Estimated liquidation price at the current risk tier.
    pub fn liquidation_price(&self, account: AccountId, symbol: SymbolId) -> Option<Price> {
        let acct = self.accounts.get(&account)?;
        let pos = acct.positions.get(&symbol)?;
        if pos.size.is_zero() {
            return None;
        }
        let m = self.market(symbol)?;
        let mark = m.mark_price;
        let tier = m.spec.tier_or_last(pos.notional(mark));
        let margin = match pos.margin_mode {
            MarginMode::Isolated => pos.isolated_margin,
            MarginMode::Cross => {
                let s = margin_summary(acct, &self.markets);
                s.equity - pos.unrealized_pnl(mark) - (s.cross_maintenance_margin - pos.maintenance_margin(mark, &m.spec))
            }
        };
        // margin + (P - entry) * size = |size| * P * mmr - maint_amount
        let num = notional(pos.entry_price, pos.size).0 as i128 - margin.0 as i128 - tier.maint_amount.0 as i128;
        let den = pos.size.0 as i128 - mul_div(pos.size.abs().0, tier.mmr.0, SCALE, Round::Down) as i128;
        if den == 0 {
            return None;
        }
        let p = (num * SCALE as i128 / den).max(0);
        Some(Price(p.min(i64::MAX as i128) as i64))
    }

    pub fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }

    // ------------------------------------------------------------------
    // Command entry points
    // ------------------------------------------------------------------

    /// Applies one command and returns the events it produced, as an owned
    /// `Vec` (one allocation per command). See [`Engine::apply`] for the
    /// allocation-free variant.
    pub fn process(&mut self, cmd: Command) -> Vec<Event> {
        self.dispatch(cmd);
        self.take_events()
    }

    /// Applies one command and returns its events as a slice of an internal
    /// buffer that is reused for every command, so no allocation happens once
    /// the buffer has grown. Events of earlier direct method calls that were
    /// not taken are discarded first.
    pub fn apply(&mut self, cmd: Command) -> &[Event] {
        self.events.clear();
        self.dispatch(cmd);
        &self.events
    }

    fn dispatch(&mut self, cmd: Command) {
        let _ = match cmd {
            Command::AddMarket { spec, price } => {
                self.add_market(spec, price);
                Ok(())
            }
            Command::Deposit { account, amount } => self.deposit(account, amount),
            Command::Withdraw { account, amount } => self.withdraw(account, amount),
            Command::FundInsurance { amount } => self.fund_insurance(amount),
            Command::SetFeeRates { account, maker, taker } => self.set_fee_rates(account, maker, taker),
            Command::PlaceOrder(o) => self.place_order(o).map(|_| ()),
            Command::CancelOrder { account, symbol, order_id } => self.cancel_order(account, symbol, order_id),
            Command::CancelAll { account, symbol } => self.cancel_all(account, symbol).map(|_| ()),
            Command::AmendOrder { account, symbol, order_id, price, qty, trigger_price } => {
                self.amend_order(account, symbol, order_id, price, qty, trigger_price)
            }
            Command::SetLeverage { account, symbol, leverage } => self.set_leverage(account, symbol, leverage),
            Command::SetMarginMode { account, symbol, mode } => self.set_margin_mode(account, symbol, mode),
            Command::AdjustIsolatedMargin { account, symbol, delta } => {
                self.adjust_isolated_margin(account, symbol, delta)
            }
            Command::SetPositionTpSl { account, symbol, take_profit, stop_loss } => {
                self.set_position_tpsl(account, symbol, take_profit, stop_loss)
            }
            Command::MarkPrice { symbol, mark, index } => self.update_mark_price(symbol, mark, index),
            Command::Funding { symbol, rate } => self.apply_funding(symbol, rate),
        };
    }

    /// Emits `CommandRejected` on error and settles cascades.
    fn finish<T>(&mut self, r: Result<T, RejectReason>) -> Result<T, RejectReason> {
        if let Err(reason) = r {
            self.events.push(Event::CommandRejected { reason });
        }
        self.settle();
        r
    }

    pub fn add_market(&mut self, mut spec: SymbolSpec, price: Price) -> SymbolId {
        let id = self.markets.len() as SymbolId;
        spec.id = id;
        self.markets.push(Market::new(spec, price));
        self.adl_rankings.push(None);
        self.events.push(Event::MarketAdded { symbol: id });
        id
    }

    pub fn deposit(&mut self, account: AccountId, amount: Amount) -> Result<(), RejectReason> {
        let r = if amount.is_pos() {
            let a = self.accounts.entry(account).or_insert_with(|| Account::new(account));
            a.balance += amount;
            let balance = a.balance;
            self.touch_risk(account);
            self.events.push(Event::BalanceChanged { account, balance, delta: amount, reason: BalanceReason::Deposit });
            Ok(())
        } else {
            Err(RejectReason::InvalidAmount)
        };
        self.finish(r)
    }

    pub fn withdraw(&mut self, account: AccountId, amount: Amount) -> Result<(), RejectReason> {
        let r = self.try_withdraw(account, amount);
        self.finish(r)
    }

    fn try_withdraw(&mut self, account: AccountId, amount: Amount) -> Result<(), RejectReason> {
        if !amount.is_pos() {
            return Err(RejectReason::InvalidAmount);
        }
        let summary = self.margin(account).ok_or(RejectReason::UnknownAccount)?;
        // Unrealized profit can't be withdrawn.
        if summary.available.min(summary.available - summary.cross_upnl.max(Amount::ZERO)) < amount {
            return Err(RejectReason::InsufficientBalance);
        }
        let a = self.accounts.get_mut(&account).expect("account");
        a.balance -= amount;
        let balance = a.balance;
        self.touch_risk(account);
        self.events.push(Event::BalanceChanged { account, balance, delta: -amount, reason: BalanceReason::Withdraw });
        Ok(())
    }

    pub fn fund_insurance(&mut self, amount: Amount) -> Result<(), RejectReason> {
        let r = if amount.is_pos() {
            self.change_insurance(amount);
            Ok(())
        } else {
            Err(RejectReason::InvalidAmount)
        };
        self.finish(r)
    }

    pub fn set_fee_rates(
        &mut self,
        account: AccountId,
        maker: Option<Rate>,
        taker: Option<Rate>,
    ) -> Result<(), RejectReason> {
        let r = match self.accounts.get_mut(&account) {
            Some(a) => {
                a.maker_fee = maker;
                a.taker_fee = taker;
                Ok(())
            }
            None => Err(RejectReason::UnknownAccount),
        };
        self.finish(r)
    }

    // ------------------------------------------------------------------
    // Orders
    // ------------------------------------------------------------------

    pub fn place_order(&mut self, req: NewOrder) -> Result<OrderId, RejectReason> {
        let r = self.try_place(&req);
        if let Err(reason) = r {
            self.events.push(Event::OrderRejected {
                client_order_id: req.client_order_id,
                account: req.account,
                symbol: req.symbol,
                reason,
            });
        }
        self.settle();
        r
    }

    fn try_place(&mut self, req: &NewOrder) -> Result<OrderId, RejectReason> {
        use RejectReason::*;
        let m = self.markets.get(req.symbol as usize).ok_or(UnknownSymbol)?;
        if !self.accounts.contains_key(&req.account) {
            return Err(UnknownAccount);
        }
        let spec = &m.spec;
        let ot = req.order_type;

        if req.close_position {
            if !matches!(ot, OrderType::StopMarket | OrderType::TakeProfitMarket | OrderType::TrailingStopMarket) {
                return Err(InvalidClosePosition);
            }
        } else {
            if !req.qty.is_pos() || req.qty < spec.min_qty || req.qty > spec.max_qty {
                return Err(InvalidQty);
            }
            if !req.qty.is_multiple_of(spec.lot_size) {
                return Err(LotSize);
            }
        }

        let limit_price = if ot.has_limit_price() {
            let p = req.price.ok_or(MissingPrice)?;
            if !p.is_pos() || !p.is_multiple_of(spec.tick_size) {
                return Err(InvalidPrice);
            }
            Some(p)
        } else {
            None
        };
        if req.tif == TimeInForce::Gtx && limit_price.is_none() {
            return Err(InvalidTif);
        }
        if let Some(d) = req.display_qty {
            if limit_price.is_none()
                || req.tif != TimeInForce::Gtc
                || !d.is_pos()
                || !d.is_multiple_of(spec.lot_size)
                || d >= req.qty
            {
                return Err(InvalidDisplayQty);
            }
        }

        let trigger = if ot.is_conditional() {
            let dir = ot.trigger_dir(req.side).expect("conditional order type");
            let current = m.price_for(req.trigger_by);
            if ot == OrderType::TrailingStopMarket {
                let t = req.trailing.ok_or(MissingTrigger)?;
                if !t.offset.is_valid() {
                    return Err(InvalidPrice);
                }
                let mut state = TrailingState {
                    offset: t.offset,
                    activation_price: t.activation_price,
                    activated: false,
                    extreme: Price::ZERO,
                };
                let reached = match (t.activation_price, req.side) {
                    (None, _) => true,
                    (Some(a), Side::Sell) => current >= a,
                    (Some(a), Side::Buy) => current <= a,
                };
                let mut price = Price::ZERO;
                if reached {
                    state.activated = true;
                    state.extreme = current;
                    price = trailing_stop_price(req.side, current, t.offset);
                }
                Some(TriggerState { price, by: req.trigger_by, dir, trailing: Some(state) })
            } else {
                let tp = req.trigger_price.ok_or(MissingTrigger)?;
                if !tp.is_pos() || !tp.is_multiple_of(spec.tick_size) {
                    return Err(InvalidPrice);
                }
                if dir.is_hit(tp, current) {
                    return Err(WouldImmediatelyTrigger);
                }
                Some(TriggerState { price: tp, by: req.trigger_by, dir, trailing: None })
            }
        } else {
            None
        };

        let ref_price = limit_price
            .or_else(|| trigger.map(|t| t.price).filter(|p| p.is_pos()))
            .unwrap_or(m.mark_price);
        if !req.reduce_only && notional(ref_price, req.qty) < spec.min_notional {
            return Err(MinNotional);
        }

        if req.take_profit.is_some() || req.stop_loss.is_some() {
            if req.reduce_only || req.close_position {
                return Err(InvalidTpSl);
            }
            let tick = spec.tick_size;
            let ok = |leg: Option<TpSl>, above: bool| {
                leg.is_none_or(|l| {
                    l.trigger_price.is_multiple_of(tick)
                        && l.limit_price.is_none_or(|p| p.is_pos() && p.is_multiple_of(tick))
                        && (l.trigger_price > ref_price) == above
                        && l.trigger_price != ref_price
                })
            };
            let long = req.side == Side::Buy;
            if !ok(req.take_profit, long) || !ok(req.stop_loss, !long) {
                return Err(InvalidTpSl);
            }
        }

        let id = self.alloc_order_id();
        let qty = if req.close_position { Qty::ZERO } else { req.qty };
        let mut o = Order::blank(id, req.account, req.symbol, req.side, ot, qty, limit_price.unwrap_or(Price::ZERO));
        o.client_order_id = req.client_order_id;
        o.tif = req.tif;
        o.reduce_only = req.reduce_only || req.close_position;
        o.close_position = req.close_position;
        o.trigger = trigger;
        o.take_profit = req.take_profit;
        o.stop_loss = req.stop_loss;
        o.display_qty = req.display_qty;
        o.stp = req.stp;

        if o.trigger.is_some() {
            o.status = OrderStatus::Untriggered;
            self.emit_accepted(&o);
            self.pos_mut(o.account, o.symbol).track(id, o.reduce_only);
            self.markets[req.symbol as usize].cond.insert(o);
        } else {
            self.submit_active(o, true)?;
        }
        Ok(id)
    }

    fn alloc_order_id(&mut self) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        id
    }

    fn emit_accepted(&mut self, o: &Order) {
        self.events.push(Event::OrderAccepted {
            order_id: o.id,
            client_order_id: o.client_order_id,
            account: o.account,
            symbol: o.symbol,
            side: o.side,
            order_type: o.order_type,
            price: o.price,
            qty: o.qty,
            status: o.status,
        });
    }

    fn emit_done(&mut self, o: &Order, reason: DoneReason) {
        self.events.push(Event::OrderDone {
            order_id: o.id,
            account: o.account,
            symbol: o.symbol,
            status: o.status,
            filled: o.filled,
            avg_price: o.avg_price(),
            reason,
        });
    }

    fn pos_mut(&mut self, account: AccountId, symbol: SymbolId) -> &mut Position {
        let lev = self.markets[symbol as usize].spec.default_leverage;
        self.accounts
            .get_mut(&account)
            .expect("account")
            .positions
            .entry(symbol)
            .or_insert_with(|| Position::new(lev))
    }

    /// Quantity an order on `side` may execute without increasing the position.
    fn reducible(&self, account: AccountId, symbol: SymbolId, side: Side) -> Qty {
        let size = self.position(account, symbol).map_or(Qty::ZERO, |p| p.size);
        match side {
            Side::Sell if size.is_pos() => size,
            Side::Buy if size.is_neg() => -size,
            _ => Qty::ZERO,
        }
    }

    fn protection_price(&self, symbol: SymbolId, side: Side) -> Price {
        let m = &self.markets[symbol as usize];
        let tick = m.spec.tick_size;
        let d = m.mark_price.mul_rate(m.spec.market_slippage, Round::Down);
        match side {
            Side::Buy => (m.mark_price + d).round_to(tick, Round::Up),
            Side::Sell => (m.mark_price - d).round_to(tick, Round::Down).max(tick),
        }
    }

    fn fee_rate(&self, account: AccountId, symbol: SymbolId, maker: bool) -> Rate {
        let spec = &self.markets[symbol as usize].spec;
        let a = self.accounts.get(&account);
        if maker {
            a.and_then(|a| a.maker_fee).unwrap_or(spec.maker_fee)
        } else {
            a.and_then(|a| a.taker_fee).unwrap_or(spec.taker_fee)
        }
    }

    /// Pre-trade check: can the account afford changing its open orders on
    /// `side` by adding `add` and removing `remove` (qty, price)?
    fn check_order_cost(
        &self,
        account: AccountId,
        symbol: SymbolId,
        side: Side,
        add: (Qty, Price),
        remove: (Qty, Price),
    ) -> Result<(), RejectReason> {
        let m = &self.markets[symbol as usize];
        let acct = self.accounts.get(&account).ok_or(RejectReason::UnknownAccount)?;
        let (size, lev, open) = acct
            .positions
            .get(&symbol)
            .map_or((Qty::ZERO, m.spec.default_leverage, Default::default()), |p| (p.size, p.leverage, p.open));
        let mut after = open;
        if remove.0.is_pos() {
            after.sub(side, remove.0, remove.1);
        }
        after.add(side, add.0, add.1);

        let projected = notional(m.mark_price, size.abs()) + after.buy_notional.max(after.sell_notional);
        let tier = m.spec.tier_for(projected).ok_or(RejectReason::RiskLimitExceeded)?;
        if lev > tier.max_leverage {
            return Err(RejectReason::RiskLimitExceeded);
        }

        let fee = notional(add.1, add.0).mul_rate(self.fee_rate(account, symbol, false), Round::Up).max(Amount::ZERO);
        let need = after.margin(size, lev) - open.margin(size, lev) + fee;
        if !need.is_pos() {
            return Ok(());
        }
        if margin_summary(acct, &self.markets).available < need {
            return Err(RejectReason::InsufficientMargin);
        }
        Ok(())
    }

    /// Validates and matches an order that is (now) active, then rests or
    /// expires the remainder. Nothing is emitted when it returns `Err`.
    fn submit_active(&mut self, mut o: Order, announce: bool) -> Result<(), RejectReason> {
        use RejectReason::*;
        let symbol = o.symbol;
        if o.reduce_only {
            let r = self.reducible(o.account, symbol, o.side);
            if !r.is_pos() {
                return Err(ReduceOnlyRejected);
            }
            if o.close_position || o.leaves() > r {
                o.qty = o.filled + r;
            }
        }

        if o.order_type.executes_as_market() {
            o.price = self.protection_price(symbol, o.side);
            if o.tif != TimeInForce::Fok {
                o.tif = TimeInForce::Ioc;
            }
        } else if !o.is_liquidation {
            let m = &self.markets[symbol as usize];
            let band = m.mark_price.mul_rate(m.spec.price_band, Round::Down);
            let out = match o.side {
                Side::Buy => o.price > m.mark_price + band,
                Side::Sell => o.price < m.mark_price - band,
            };
            if out {
                return Err(PriceOutOfBand);
            }
        }

        let m = &self.markets[symbol as usize];
        if o.tif == TimeInForce::Gtx {
            if let Some(best) = m.book.best_price(o.side.opposite()) {
                if crosses(o.side, o.price, best) {
                    return Err(PostOnlyWouldTake);
                }
            }
        }
        if o.tif == TimeInForce::Fok {
            let exclude = (o.stp != StpMode::None).then_some(o.account);
            let stop_at_own = matches!(o.stp, StpMode::CancelTaker | StpMode::CancelBoth);
            if m.book.fillable(o.side, o.price, exclude, stop_at_own, o.leaves()) < o.leaves() {
                return Err(FokNotFillable);
            }
        }
        if !o.reduce_only && !o.is_liquidation {
            self.check_order_cost(o.account, symbol, o.side, (o.leaves(), o.price), (Qty::ZERO, Price::ZERO))?;
        }

        o.status = OrderStatus::New;
        if announce {
            self.emit_accepted(&o);
        }
        let end = self.match_order(&mut o);
        self.finish_taker(o, end);
        Ok(())
    }

    fn finish_taker(&mut self, mut o: Order, end: MatchEnd) {
        let leaves = o.leaves();
        if leaves.is_zero() {
            o.status = OrderStatus::Filled;
            self.emit_done(&o, DoneReason::Filled);
            return;
        }
        let rests = end == MatchEnd::Done
            && matches!(o.tif, TimeInForce::Gtc | TimeInForce::Gtx)
            && !o.order_type.executes_as_market();
        if rests {
            o.visible = o.display_qty.map_or(leaves, |d| d.min(leaves));
            o.status = if o.filled.is_pos() { OrderStatus::PartiallyFilled } else { OrderStatus::New };
            let (account, symbol, side, price, id, ro) = (o.account, o.symbol, o.side, o.price, o.id, o.reduce_only);
            let pos = self.pos_mut(account, symbol);
            if !ro {
                pos.open.add(side, leaves, price);
            }
            pos.track(id, ro);
            self.markets[symbol as usize].book.insert(o);
        } else {
            let reason = match end {
                MatchEnd::SelfTrade => DoneReason::SelfTrade,
                MatchEnd::ReduceOnly => DoneReason::ReduceOnly,
                MatchEnd::Done => DoneReason::Expired,
            };
            o.status = if reason == DoneReason::Expired { OrderStatus::Expired } else { OrderStatus::Canceled };
            self.emit_done(&o, reason);
        }
    }

    fn match_order(&mut self, taker: &mut Order) -> MatchEnd {
        let symbol = taker.symbol;
        let opp = taker.side.opposite();
        while taker.leaves().is_pos() {
            let book = &self.markets[symbol as usize].book;
            let Some(best) = book.best_price(opp) else { break };
            if !crosses(taker.side, taker.price, best) {
                break;
            }
            let mo = book.front(opp, best).expect("non-empty level");
            let (maker_id, maker_account, maker_visible, maker_ro) = (mo.id, mo.account, mo.visible, mo.reduce_only());

            if maker_account == taker.account && !taker.is_liquidation {
                match taker.stp {
                    StpMode::None => {}
                    StpMode::CancelMaker => {
                        self.cancel_by_id(symbol, maker_id, DoneReason::SelfTrade);
                        continue;
                    }
                    StpMode::CancelTaker => return MatchEnd::SelfTrade,
                    StpMode::CancelBoth => {
                        self.cancel_by_id(symbol, maker_id, DoneReason::SelfTrade);
                        return MatchEnd::SelfTrade;
                    }
                }
            }

            let mut qty = taker.leaves().min(maker_visible);
            if taker.reduce_only {
                let cap = self.reducible(taker.account, symbol, taker.side);
                if !cap.is_pos() {
                    return MatchEnd::ReduceOnly;
                }
                qty = qty.min(cap);
            }
            if maker_ro {
                let cap = self.reducible(maker_account, symbol, opp);
                if !cap.is_pos() {
                    self.cancel_by_id(symbol, maker_id, DoneReason::ReduceOnly);
                    continue;
                }
                qty = qty.min(cap);
            }
            self.execute(taker, maker_id, best, qty);
        }
        MatchEnd::Done
    }

    fn execute(&mut self, taker: &mut Order, maker_id: OrderId, price: Price, qty: Qty) {
        let symbol = taker.symbol;
        let si = symbol as usize;
        let n = notional(price, qty);
        let maker_account = self.markets[si].book.resting(maker_id).expect("maker").account;
        let maker_fee = n.mul_rate(self.fee_rate(maker_account, symbol, true), Round::Up);
        let taker_fee = n.mul_rate(self.fee_rate(taker.account, symbol, false), Round::Up);

        let m = &mut self.markets[si];
        let mo = m.book.fill(maker_id, qty, n);
        let (maker_side, maker_ro, maker_leaves, maker_visible, maker_attached) =
            (mo.side, mo.reduce_only(), mo.leaves(), mo.visible, mo.has_attachments());
        m.last_price = price;
        taker.filled += qty;
        taker.cum_quote += n;

        let trade_id = self.next_trade_id;
        self.next_trade_id += 1;
        self.events.push(Event::Trade {
            trade_id,
            symbol,
            price,
            qty,
            taker_side: taker.side,
            taker_order: taker.id,
            maker_order: maker_id,
            taker_account: taker.account,
            maker_account,
            taker_fee,
            maker_fee,
            liquidation: taker.is_liquidation,
        });

        if !maker_ro {
            self.pos_mut(maker_account, symbol).open.sub(maker_side, qty, price);
        }
        self.apply_fill(taker.account, symbol, taker.side, price, qty, taker_fee);
        self.apply_fill(maker_account, symbol, maker_side, price, qty, maker_fee);

        if taker.has_attachments() {
            let (tp, sl) = (taker.take_profit, taker.stop_loss);
            self.attach_on_fill(taker.account, symbol, taker.side, taker.id, tp, sl, &mut taker.children, qty);
        }
        if maker_attached {
            let mo = self.markets[si].book.meta(maker_id).expect("maker");
            let (tp, sl, mut children) = (mo.take_profit, mo.stop_loss, mo.children);
            self.attach_on_fill(maker_account, symbol, maker_side, maker_id, tp, sl, &mut children, qty);
            self.markets[si].book.meta_mut(maker_id).expect("maker").children = children;
        }

        if maker_leaves.is_zero() {
            let mut mo = self.markets[si].book.remove(maker_id).expect("maker");
            mo.status = OrderStatus::Filled;
            self.pos_mut(maker_account, symbol).untrack(maker_id);
            self.emit_done(&mo, DoneReason::Filled);
        } else {
            let book = &mut self.markets[si].book;
            if maker_visible.is_zero() {
                book.replenish(maker_id);
            }
            book.meta_mut(maker_id).expect("maker").status = OrderStatus::PartiallyFilled;
        }
        self.mark_dirty(symbol);
    }

    /// Books a fill into the account's position and balances.
    fn apply_fill(&mut self, account: AccountId, symbol: SymbolId, side: Side, price: Price, qty: Qty, fee: Amount) {
        if self.liq_pass.is_some() {
            self.note_pass_touch(account);
        }
        let default_lev = self.markets[symbol as usize].spec.default_leverage;
        let a = self.accounts.get_mut(&account).expect("account");
        let newly_dirty = !a.risk_dirty;
        a.risk_dirty = true;
        let pos = a.positions.entry(symbol).or_insert_with(|| Position::new(default_lev));
        let old = pos.size;
        let isolated = pos.margin_mode == MarginMode::Isolated;
        let (close, open) = if old.is_zero() || old.signum() == side.sign() {
            (Qty::ZERO, qty)
        } else {
            let c = qty.min(old.abs());
            (c, qty - c)
        };

        let mut realized = Amount::ZERO;
        if close.is_pos() {
            realized = notional(price - pos.entry_price, Qty(close.0 * old.signum()));
            pos.size += side.signed(close);
            if isolated {
                let released = Amount(mul_div(pos.isolated_margin.0, close.0, old.abs().0, Round::Down));
                pos.isolated_margin -= released;
                let net = released + realized;
                if net.is_neg() {
                    // Loss beyond the released share is borne by the remaining collateral.
                    pos.isolated_margin += net;
                } else {
                    a.balance += net;
                }
            } else {
                a.balance += realized;
            }
            if pos.size.is_zero() {
                pos.entry_price = Price::ZERO;
            }
        }
        if open.is_pos() {
            let cur = pos.size.abs();
            pos.entry_price = if cur.is_zero() {
                price
            } else {
                let w = pos.entry_price.0 as i128 * cur.0 as i128 + price.0 as i128 * open.0 as i128;
                Price((w / (cur.0 + open.0) as i128) as i64)
            };
            pos.size += side.signed(open);
            if isolated {
                let im = notional(price, open).div_leverage(pos.leverage, Round::Up);
                a.balance -= im;
                pos.isolated_margin += im;
            }
        }
        a.balance -= fee;
        pos.realized_pnl += realized;
        pos.fees_paid += fee;

        let mut deficit = Amount::ZERO;
        if isolated && pos.size.is_zero() && !pos.isolated_margin.is_zero() {
            let rest = pos.isolated_margin;
            pos.isolated_margin = Amount::ZERO;
            if rest.is_pos() {
                a.balance += rest;
            } else {
                deficit = -rest;
            }
        }
        let (size, entry_price) = (pos.size, pos.entry_price);
        if let Some(ranking) = self.adl_rankings[symbol as usize].as_mut() {
            update_ranking(ranking, account, pos, self.markets[symbol as usize].mark_price);
        }

        let holders = &mut self.markets[symbol as usize].holders;
        if size.is_zero() {
            holders.remove(&account);
        } else {
            holders.insert(account);
        }
        if deficit.is_pos() {
            self.change_insurance(-deficit);
        }
        if newly_dirty {
            self.risk_dirty.push(account);
        }
        self.events.push(Event::PositionChanged { account, symbol, size, entry_price, realized_pnl: realized, fee });
        self.touched.push((account, symbol));
    }

    fn change_insurance(&mut self, delta: Amount) {
        self.insurance_fund += delta;
        self.events.push(Event::InsuranceFundChanged { balance: self.insurance_fund, delta });
    }

    /// Creates (or grows) the reduce-only TP/SL children of a parent order as
    /// it fills. Children are OCO-linked.
    #[allow(clippy::too_many_arguments)]
    fn attach_on_fill(
        &mut self,
        account: AccountId,
        symbol: SymbolId,
        parent_side: Side,
        parent_id: OrderId,
        tp: Option<TpSl>,
        sl: Option<TpSl>,
        children: &mut [Option<OrderId>; 2],
        qty: Qty,
    ) {
        let si = symbol as usize;
        let legs = [tp, sl];
        let cond = &mut self.markets[si].cond;
        let all_alive = (0..2).all(|k| legs[k].is_none() || children[k].is_some_and(|id| cond.contains(id)));
        if all_alive && children.iter().any(Option::is_some) {
            for id in children.iter().flatten() {
                cond.get_mut(*id).expect("child").qty += qty;
            }
            return;
        }

        let side = parent_side.opposite();
        let mut ids = [None, None];
        let mut orders = Vec::with_capacity(2);
        for (k, leg) in legs.into_iter().enumerate() {
            let Some(leg) = leg else { continue };
            let ot = match (k, leg.limit_price.is_some()) {
                (0, false) => OrderType::TakeProfitMarket,
                (0, true) => OrderType::TakeProfitLimit,
                (_, false) => OrderType::StopMarket,
                (_, true) => OrderType::StopLimit,
            };
            let id = self.alloc_order_id();
            let mut o = Order::blank(id, account, symbol, side, ot, qty, leg.limit_price.unwrap_or(Price::ZERO));
            o.reduce_only = true;
            o.status = OrderStatus::Untriggered;
            o.origin = OrderOrigin::AttachedTpSl { parent: parent_id };
            o.trigger = Some(TriggerState {
                price: leg.trigger_price,
                by: leg.trigger_by,
                dir: ot.trigger_dir(side).expect("conditional"),
                trailing: None,
            });
            ids[k] = Some(id);
            orders.push(o);
        }
        if let [a, b] = orders.as_mut_slice() {
            a.linked = Some(b.id);
            b.linked = Some(a.id);
        }
        for o in orders {
            self.emit_accepted(&o);
            self.pos_mut(account, symbol).track(o.id, true);
            self.markets[si].cond.insert(o);
        }
        *children = ids;
        self.mark_dirty(symbol);
    }

    /// Removes a live order (resting or conditional) and reports it.
    fn cancel_by_id(&mut self, symbol: SymbolId, id: OrderId, reason: DoneReason) -> Option<Order> {
        let si = symbol as usize;
        let mut o = if let Some(o) = self.markets[si].book.remove(id) {
            if !o.reduce_only {
                self.pos_mut(o.account, symbol).open.sub(o.side, o.leaves(), o.price);
            }
            o
        } else {
            self.markets[si].cond.remove(id)?
        };
        self.pos_mut(o.account, symbol).untrack(id);
        o.status = OrderStatus::Canceled;
        self.emit_done(&o, reason);
        Some(o)
    }

    pub fn cancel_order(&mut self, account: AccountId, symbol: SymbolId, order_id: OrderId) -> Result<(), RejectReason> {
        let r = match self.order(symbol, order_id) {
            Some(o) if o.account == account => {
                self.cancel_by_id(symbol, order_id, DoneReason::UserCanceled);
                Ok(())
            }
            _ if self.market(symbol).is_none() => Err(RejectReason::UnknownSymbol),
            _ => Err(RejectReason::UnknownOrder),
        };
        self.finish(r)
    }

    /// Cancels all of an account's orders, in one symbol or everywhere.
    pub fn cancel_all(&mut self, account: AccountId, symbol: Option<SymbolId>) -> Result<usize, RejectReason> {
        let r = if self.accounts.contains_key(&account) {
            Ok(self.cancel_account_orders(account, symbol, DoneReason::UserCanceled))
        } else {
            Err(RejectReason::UnknownAccount)
        };
        self.finish(r)
    }

    fn cancel_account_orders(&mut self, account: AccountId, symbol: Option<SymbolId>, reason: DoneReason) -> usize {
        let Some(acct) = self.accounts.get(&account) else { return 0 };
        let targets: Vec<(SymbolId, OrderId)> = acct
            .positions
            .iter()
            .filter(|(s, _)| symbol.is_none_or(|x| x == **s))
            .flat_map(|(s, p)| p.order_ids.iter().map(move |id| (*s, *id)))
            .collect();
        for &(s, id) in &targets {
            self.cancel_by_id(s, id, reason);
        }
        targets.len()
    }

    pub fn amend_order(
        &mut self,
        account: AccountId,
        symbol: SymbolId,
        order_id: OrderId,
        price: Option<Price>,
        qty: Option<Qty>,
        trigger_price: Option<Price>,
    ) -> Result<(), RejectReason> {
        let r = self.try_amend(account, symbol, order_id, price, qty, trigger_price);
        self.finish(r)
    }

    fn try_amend(
        &mut self,
        account: AccountId,
        symbol: SymbolId,
        id: OrderId,
        price: Option<Price>,
        qty: Option<Qty>,
        trigger_price: Option<Price>,
    ) -> Result<(), RejectReason> {
        use RejectReason::*;
        let si = symbol as usize;
        let m = self.markets.get(si).ok_or(UnknownSymbol)?;
        let (tick, lot) = (m.spec.tick_size, m.spec.lot_size);
        if price.is_some_and(|p| !p.is_pos() || !p.is_multiple_of(tick)) {
            return Err(InvalidPrice);
        }
        if trigger_price.is_some_and(|p| !p.is_pos() || !p.is_multiple_of(tick)) {
            return Err(InvalidPrice);
        }
        if qty.is_some_and(|q| !q.is_pos() || !q.is_multiple_of(lot)) {
            return Err(LotSize);
        }

        if let Some(o) = m.book.order(id) {
            if o.account != account {
                return Err(UnknownOrder);
            }
            if trigger_price.is_some() {
                return Err(InvalidPrice);
            }
            let new_price = price.unwrap_or(o.price);
            let new_qty = qty.unwrap_or(o.qty);
            if new_qty <= o.filled || new_qty > m.spec.max_qty {
                return Err(InvalidQty);
            }
            let (side, old_price, old_qty, old_leaves, ro, tif) =
                (o.side, o.price, o.qty, o.leaves(), o.reduce_only, o.tif);
            let new_leaves = new_qty - o.filled;

            // Size-down at the same price keeps queue priority.
            if new_price == old_price && new_qty <= old_qty {
                self.markets[si].book.reduce_qty(id, new_qty);
                if !ro {
                    self.pos_mut(account, symbol).open.sub(side, old_leaves - new_leaves, old_price);
                }
                self.events.push(Event::OrderAmended {
                    order_id: id,
                    account,
                    symbol,
                    price: new_price,
                    qty: new_qty,
                    trigger_price: None,
                });
                return Ok(());
            }

            let band = m.mark_price.mul_rate(m.spec.price_band, Round::Down);
            let out_of_band = match side {
                Side::Buy => new_price > m.mark_price + band,
                Side::Sell => new_price < m.mark_price - band,
            };
            if out_of_band {
                return Err(PriceOutOfBand);
            }
            if tif == TimeInForce::Gtx
                && m.book.best_price(side.opposite()).is_some_and(|b| crosses(side, new_price, b))
            {
                return Err(PostOnlyWouldTake);
            }
            if !ro {
                self.check_order_cost(account, symbol, side, (new_leaves, new_price), (old_leaves, old_price))?;
            }

            // Otherwise the order is re-entered and loses priority.
            let mut o = self.markets[si].book.remove(id).expect("order");
            let pos = self.pos_mut(account, symbol);
            if !ro {
                pos.open.sub(side, old_leaves, old_price);
            }
            pos.untrack(id);
            o.price = new_price;
            o.qty = new_qty;
            self.events.push(Event::OrderAmended {
                order_id: id,
                account,
                symbol,
                price: new_price,
                qty: new_qty,
                trigger_price: None,
            });
            let end = self.match_order(&mut o);
            self.finish_taker(o, end);
            Ok(())
        } else if let Some(o) = m.cond.get(id) {
            if o.account != account {
                return Err(UnknownOrder);
            }
            let t = o.trigger.expect("trigger");
            if price.is_some() && !o.order_type.has_limit_price() {
                return Err(InvalidPrice);
            }
            if qty.is_some() && o.close_position {
                return Err(InvalidQty);
            }
            if let Some(tp) = trigger_price {
                if t.trailing.is_some() {
                    return Err(InvalidPrice);
                }
                if t.dir.is_hit(tp, m.price_for(t.by)) {
                    return Err(WouldImmediatelyTrigger);
                }
            }
            let cond = &mut self.markets[si].cond;
            let mut o = cond.remove(id).expect("order");
            if let Some(p) = price {
                o.price = p;
            }
            if let Some(q) = qty {
                o.qty = q;
            }
            if let Some(tp) = trigger_price {
                o.trigger.as_mut().expect("trigger").price = tp;
            }
            self.events.push(Event::OrderAmended {
                order_id: id,
                account,
                symbol,
                price: o.price,
                qty: o.qty,
                trigger_price: o.trigger.map(|t| t.price),
            });
            cond.insert(o);
            Ok(())
        } else {
            Err(UnknownOrder)
        }
    }

    // ------------------------------------------------------------------
    // Position settings
    // ------------------------------------------------------------------

    pub fn set_leverage(&mut self, account: AccountId, symbol: SymbolId, leverage: u32) -> Result<(), RejectReason> {
        let r = self.try_set_leverage(account, symbol, leverage);
        self.finish(r)
    }

    fn try_set_leverage(&mut self, account: AccountId, symbol: SymbolId, leverage: u32) -> Result<(), RejectReason> {
        use RejectReason::*;
        let m = self.markets.get(symbol as usize).ok_or(UnknownSymbol)?;
        if !self.accounts.contains_key(&account) {
            return Err(UnknownAccount);
        }
        if leverage == 0 || leverage > m.spec.max_leverage() {
            return Err(InvalidLeverage);
        }
        let mark = m.mark_price;
        let pos = self.pos_mut(account, symbol);
        let projected = pos.notional(mark) + pos.open.buy_notional.max(pos.open.sell_notional);
        let old = pos.leverage;
        let (mode, size, iso) = (pos.margin_mode, pos.size, pos.isolated_margin);
        let tier_max = self.markets[symbol as usize].spec.tier_for(projected).map(|t| t.max_leverage);
        if tier_max.is_none_or(|t| leverage > t) {
            return Err(RiskLimitExceeded);
        }

        self.pos_mut(account, symbol).leverage = leverage;
        let mut top_up = Amount::ZERO;
        if mode == MarginMode::Isolated && !size.is_zero() {
            let need = self.pos_mut(account, symbol).initial_margin(mark);
            top_up = (need - iso).max(Amount::ZERO);
        }
        if leverage < old {
            let available = self.margin(account).expect("account").available;
            if available < top_up {
                self.pos_mut(account, symbol).leverage = old;
                return Err(InsufficientMargin);
            }
        }
        if top_up.is_pos() {
            self.accounts.get_mut(&account).expect("account").balance -= top_up;
            self.touch_risk(account);
            let pos = self.pos_mut(account, symbol);
            pos.isolated_margin += top_up;
            let isolated_margin = pos.isolated_margin;
            self.events.push(Event::IsolatedMarginChanged { account, symbol, isolated_margin, delta: top_up });
        }
        self.events.push(Event::LeverageChanged { account, symbol, leverage });
        Ok(())
    }

    pub fn set_margin_mode(&mut self, account: AccountId, symbol: SymbolId, mode: MarginMode) -> Result<(), RejectReason> {
        let r = self.try_set_margin_mode(account, symbol, mode);
        self.finish(r)
    }

    fn try_set_margin_mode(&mut self, account: AccountId, symbol: SymbolId, mode: MarginMode) -> Result<(), RejectReason> {
        if self.markets.get(symbol as usize).is_none() {
            return Err(RejectReason::UnknownSymbol);
        }
        if !self.accounts.contains_key(&account) {
            return Err(RejectReason::UnknownAccount);
        }
        let pos = self.pos_mut(account, symbol);
        if pos.margin_mode == mode {
            return Ok(());
        }
        if !pos.size.is_zero() || !pos.order_ids.is_empty() {
            return Err(RejectReason::PositionOrOrdersOpen);
        }
        pos.margin_mode = mode;
        self.events.push(Event::MarginModeChanged { account, symbol, mode });
        Ok(())
    }

    /// Adds (`delta > 0`) or removes collateral of an isolated position.
    pub fn adjust_isolated_margin(&mut self, account: AccountId, symbol: SymbolId, delta: Amount) -> Result<(), RejectReason> {
        let r = self.try_adjust_isolated_margin(account, symbol, delta);
        self.finish(r)
    }

    fn try_adjust_isolated_margin(&mut self, account: AccountId, symbol: SymbolId, delta: Amount) -> Result<(), RejectReason> {
        use RejectReason::*;
        let mark = self.markets.get(symbol as usize).ok_or(UnknownSymbol)?.mark_price;
        let pos = self.position(account, symbol).ok_or(NoPosition)?;
        if pos.margin_mode != MarginMode::Isolated {
            return Err(NotIsolated);
        }
        if pos.size.is_zero() {
            return Err(NoPosition);
        }
        if delta.is_zero() {
            return Err(InvalidAmount);
        }
        if delta.is_pos() {
            if self.margin(account).expect("account").available < delta {
                return Err(InsufficientMargin);
            }
        } else {
            let left = pos.isolated_margin + delta + pos.unrealized_pnl(mark).min(Amount::ZERO);
            if left < pos.initial_margin(mark) {
                return Err(InsufficientMargin);
            }
        }
        self.accounts.get_mut(&account).expect("account").balance -= delta;
        self.touch_risk(account);
        let pos = self.pos_mut(account, symbol);
        pos.isolated_margin += delta;
        let isolated_margin = pos.isolated_margin;
        self.events.push(Event::IsolatedMarginChanged { account, symbol, isolated_margin, delta });
        Ok(())
    }

    /// Sets position-level TP/SL that close the whole position (OCO).
    /// Passing `None` for a leg removes it.
    pub fn set_position_tpsl(
        &mut self,
        account: AccountId,
        symbol: SymbolId,
        take_profit: Option<TpSl>,
        stop_loss: Option<TpSl>,
    ) -> Result<(), RejectReason> {
        let r = self.try_set_position_tpsl(account, symbol, take_profit, stop_loss);
        self.finish(r)
    }

    fn try_set_position_tpsl(
        &mut self,
        account: AccountId,
        symbol: SymbolId,
        take_profit: Option<TpSl>,
        stop_loss: Option<TpSl>,
    ) -> Result<(), RejectReason> {
        use RejectReason::*;
        let m = self.markets.get(symbol as usize).ok_or(UnknownSymbol)?;
        let pos = self.position(account, symbol).ok_or(NoPosition)?;
        let close_side = pos.side().ok_or(NoPosition)?.opposite();
        let tick = m.spec.tick_size;
        let legs = [
            take_profit.map(|l| (l, if l.limit_price.is_some() { OrderType::TakeProfitLimit } else { OrderType::TakeProfitMarket })),
            stop_loss.map(|l| (l, if l.limit_price.is_some() { OrderType::StopLimit } else { OrderType::StopMarket })),
        ];
        for (leg, ot) in legs.iter().flatten() {
            if !leg.trigger_price.is_pos()
                || !leg.trigger_price.is_multiple_of(tick)
                || leg.limit_price.is_some_and(|p| !p.is_pos() || !p.is_multiple_of(tick))
            {
                return Err(InvalidPrice);
            }
            let dir = ot.trigger_dir(close_side).expect("conditional");
            if dir.is_hit(leg.trigger_price, m.price_for(leg.trigger_by)) {
                return Err(WouldImmediatelyTrigger);
            }
        }

        let (old_tp, old_sl) = (pos.tp_order, pos.sl_order);
        for id in [old_tp, old_sl].into_iter().flatten() {
            self.cancel_by_id(symbol, id, DoneReason::Replaced);
        }
        let mut ids = [None, None];
        let mut orders = Vec::with_capacity(2);
        for (k, leg) in legs.into_iter().enumerate() {
            let Some((leg, ot)) = leg else { continue };
            let id = self.alloc_order_id();
            let mut o = Order::blank(id, account, symbol, close_side, ot, Qty::ZERO, leg.limit_price.unwrap_or(Price::ZERO));
            o.reduce_only = true;
            o.close_position = true;
            o.status = OrderStatus::Untriggered;
            o.origin = OrderOrigin::PositionTpSl;
            o.trigger = Some(TriggerState {
                price: leg.trigger_price,
                by: leg.trigger_by,
                dir: ot.trigger_dir(close_side).expect("conditional"),
                trailing: None,
            });
            ids[k] = Some(id);
            orders.push(o);
        }
        if let [a, b] = orders.as_mut_slice() {
            a.linked = Some(b.id);
            b.linked = Some(a.id);
        }
        for o in orders {
            self.emit_accepted(&o);
            self.pos_mut(account, symbol).track(o.id, true);
            self.markets[symbol as usize].cond.insert(o);
        }
        let pos = self.pos_mut(account, symbol);
        pos.tp_order = ids[0];
        pos.sl_order = ids[1];
        self.events.push(Event::PositionTpSlSet { account, symbol, take_profit: ids[0], stop_loss: ids[1] });
        Ok(())
    }

    // ------------------------------------------------------------------
    // Market data: mark price, funding
    // ------------------------------------------------------------------

    /// New mark/index price: fires mark-price triggers, then liquidates
    /// positions below maintenance margin.
    pub fn update_mark_price(&mut self, symbol: SymbolId, mark: Price, index: Price) -> Result<(), RejectReason> {
        let r = match self.markets.get_mut(symbol as usize) {
            None => Err(RejectReason::UnknownSymbol),
            Some(_) if !mark.is_pos() => Err(RejectReason::InvalidPrice),
            Some(m) => {
                m.mark_price = mark;
                m.index_price = index;
                self.events.push(Event::MarkPrice { symbol, mark, index });
                self.mark_dirty(symbol);
                self.settle();
                self.check_liquidations(symbol);
                Ok(())
            }
        };
        self.finish(r)
    }

    /// Exchanges funding between longs and shorts at `rate` (positive: longs pay).
    pub fn apply_funding(&mut self, symbol: SymbolId, rate: Rate) -> Result<(), RejectReason> {
        let r = match self.markets.get(symbol as usize) {
            None => Err(RejectReason::UnknownSymbol),
            Some(m) => {
                let mark = m.mark_price;
                let holders: Vec<AccountId> = m.holders.iter().copied().collect();
                for account in holders {
                    let a = self.accounts.get_mut(&account).expect("account");
                    let pos = a.positions.get_mut(&symbol).expect("position");
                    let amount = notional(mark, pos.size).mul_rate(rate, Round::Down);
                    // Debits come out of the risk headroom; the bands stay valid
                    // until it runs out (see docs/liquidation-index.md).
                    let headroom = match pos.margin_mode {
                        MarginMode::Isolated => {
                            pos.isolated_margin -= amount;
                            pos.risk_headroom -= amount;
                            pos.risk_headroom
                        }
                        MarginMode::Cross => {
                            a.balance -= amount;
                            a.risk_headroom -= amount;
                            a.risk_headroom
                        }
                    };
                    pos.funding_paid += amount;
                    if headroom.is_neg() {
                        self.touch_risk(account);
                    }
                    self.events.push(Event::FundingPayment { account, symbol, rate, amount });
                }
                self.check_liquidations(symbol);
                Ok(())
            }
        };
        self.finish(r)
    }

    // ------------------------------------------------------------------
    // Cascades: triggers and reduce-only maintenance
    // ------------------------------------------------------------------

    fn mark_dirty(&mut self, symbol: SymbolId) {
        if !self.dirty.contains(&symbol) {
            self.dirty.push(symbol);
        }
    }

    /// Runs trigger scans and reduce-only maintenance until nothing changes.
    fn settle(&mut self) {
        for _ in 0..MAX_SETTLE_STEPS {
            if let Some(symbol) = self.dirty.pop() {
                self.run_triggers(symbol);
                continue;
            }
            if !self.touched.is_empty() {
                let spare = std::mem::take(&mut self.touched_spare);
                let mut touched = std::mem::replace(&mut self.touched, spare);
                touched.sort_unstable();
                touched.dedup();
                for &(account, symbol) in &touched {
                    self.sync_reduce_only(account, symbol);
                }
                touched.clear();
                self.touched_spare = touched;
                continue;
            }
            return;
        }
    }

    fn run_triggers(&mut self, symbol: SymbolId) {
        let si = symbol as usize;
        let mut fired = Vec::new();
        {
            let m = &mut self.markets[si];
            let (last, mark) = (m.last_price, m.mark_price);
            m.cond.collect_triggered(TriggerBy::LastPrice, last, &mut fired);
            m.cond.collect_triggered(TriggerBy::MarkPrice, mark, &mut fired);
        }
        fired.sort_unstable();
        fired.dedup();
        for id in fired {
            // An earlier trigger in this batch may have canceled it (OCO).
            let Some(mut o) = self.markets[si].cond.remove(id) else { continue };
            self.pos_mut(o.account, symbol).untrack(id);
            if let Some(linked) = o.linked {
                self.cancel_by_id(symbol, linked, DoneReason::OcoSibling);
            }
            let t = o.trigger.expect("trigger");
            self.events.push(Event::OrderTriggered {
                order_id: id,
                account: o.account,
                symbol,
                trigger_price: t.price,
                market_price: self.markets[si].price_for(t.by),
            });
            o.status = OrderStatus::New;
            if let Err(reason) = self.submit_active(o.clone(), false) {
                o.status = OrderStatus::Rejected;
                self.emit_done(&o, DoneReason::Rejected(reason));
            }
        }
    }

    /// Keeps reduce-only orders consistent with the position: cancels orders
    /// on the wrong side or beyond the position size, and drops conditional
    /// close orders once the position is flat.
    fn sync_reduce_only(&mut self, account: AccountId, symbol: SymbolId) {
        let Some(pos) = self.position(account, symbol) else { return };
        if pos.reduce_only_ids.is_empty() {
            return;
        }
        let closing_side = pos.side().map(Side::opposite);
        let mut budget = pos.size.abs();
        let m = &self.markets[symbol as usize];
        let mut cancel = Vec::new();
        for &id in &pos.reduce_only_ids {
            if let Some(o) = m.book.resting(id) {
                if Some(o.side) != closing_side || o.leaves() > budget {
                    cancel.push(id);
                } else {
                    budget -= o.leaves();
                }
            } else if closing_side.is_none() {
                cancel.push(id);
            }
        }
        let reason = if closing_side.is_none() { DoneReason::PositionClosed } else { DoneReason::ReduceOnly };
        for id in cancel {
            self.cancel_by_id(symbol, id, reason);
        }
    }

    // ------------------------------------------------------------------
    // Liquidation and auto-deleveraging
    // ------------------------------------------------------------------

    /// Marks an account's risk bands stale, and records the first touch of
    /// an account during a liquidation pass.
    fn touch_risk(&mut self, account: AccountId) {
        if self.liq_pass.is_some() {
            self.note_pass_touch(account);
        }
        self.mark_risk_dirty(account);
    }

    fn mark_risk_dirty(&mut self, account: AccountId) {
        if let Some(a) = self.accounts.get_mut(&account)
            && !a.risk_dirty
        {
            a.risk_dirty = true;
            self.risk_dirty.push(account);
        }
    }

    /// During a liquidation pass, records the first touch of an account
    /// together with whether it held the pass symbol when the pass started.
    #[cold]
    #[inline(never)]
    fn note_pass_touch(&mut self, account: AccountId) {
        if let Some(pass) = self.liq_pass.as_mut()
            && pass.seen.insert(account)
        {
            let was_holder = self.markets[pass.symbol as usize].holders.contains(&account);
            pass.touched.push((account, was_holder));
        }
    }

    /// Liquidates holders of `symbol` that are below maintenance margin.
    ///
    /// Equivalent to visiting every holder in `AccountId` order (the reference
    /// behaviour), but only candidates from the risk index, dirty accounts and
    /// accounts touched during the pass are visited. See `docs/liquidation-index.md`.
    fn check_liquidations(&mut self, symbol: SymbolId) {
        let si = symbol as usize;
        let mut pending = BTreeSet::new();
        {
            let m = &self.markets[si];
            m.risk.collect(m.mark_price, &mut pending);
            pending.extend(self.risk_dirty.iter().copied());
            pending.retain(|a| m.holders.contains(a));
        }
        self.liq_pass = Some(LiqPass { symbol, seen: FastSet::default(), touched: Vec::new() });

        while let Some(account) = pending.pop_first() {
            // Re-arm afterwards: its band was breached or its state is stale.
            self.touch_risk(account);
            if let Some(pos) = self.position(account, symbol).filter(|p| !p.size.is_zero()) {
                match pos.margin_mode {
                    MarginMode::Isolated => {
                        if self.isolated_at_risk(account, symbol) {
                            self.liquidate_isolated(account, symbol);
                        }
                    }
                    MarginMode::Cross => {
                        if self.cross_at_risk(account) {
                            self.liquidate_cross(account);
                        }
                    }
                }
                self.settle();
            }
            // Holders still ahead of the cursor whose state just changed.
            let pass = self.liq_pass.as_mut().expect("liquidation pass");
            for (x, was_holder) in pass.touched.drain(..) {
                if was_holder && x > account {
                    pending.insert(x);
                }
            }
        }

        self.liq_pass = None;
        for ranking in &mut self.adl_rankings {
            *ranking = None;
        }
        self.rearm_dirty();
        #[cfg(any(debug_assertions, feature = "risk-oracle"))]
        self.verify_risk_index();
    }

    /// Recomputes the risk bands of every dirty account at the current marks.
    fn rearm_dirty(&mut self) {
        let mut dirty = std::mem::take(&mut self.risk_dirty);
        let mut armed = std::mem::take(&mut self.armed_buf);
        for &account in &dirty {
            let Some(acct) = self.accounts.get_mut(&account) else { continue };
            acct.risk_dirty = false;
            for (&sym, pos) in acct.positions.iter_mut() {
                if let Some(band) = pos.risk_band.take() {
                    self.markets[sym as usize].risk.remove(account, band);
                }
            }
            compute_bands(acct, &self.markets, &mut armed);
            acct.risk_headroom = armed.cross_headroom;
            for &(sym, band, headroom) in &armed.bands {
                let pos = acct.positions.get_mut(&sym).expect("position");
                pos.risk_band = Some(band);
                pos.risk_headroom = headroom;
                self.markets[sym as usize].risk.insert(account, band);
            }
        }
        dirty.clear();
        self.risk_dirty = dirty;
        self.armed_buf = armed;
    }

    /// Reference oracle: full scan asserting the index missed no account.
    #[cfg(any(debug_assertions, feature = "risk-oracle"))]
    fn verify_risk_index(&self) {
        use crate::risk::ALWAYS;
        assert!(self.risk_dirty.is_empty(), "dirty accounts left after a liquidation pass");
        for (&account, acct) in &self.accounts {
            let cross_risk = self.cross_at_risk(account);
            for (&sym, pos) in &acct.positions {
                if pos.size.is_zero() {
                    assert!(pos.risk_band.is_none(), "flat position {account}/{sym} still indexed");
                    continue;
                }
                let m = &self.markets[sym as usize];
                let band = pos.risk_band.unwrap_or_else(|| panic!("position {account}/{sym} not indexed"));
                assert!(m.risk.contains(account, band), "band of {account}/{sym} missing from the index");
                let at_risk = match pos.margin_mode {
                    MarginMode::Isolated => self.isolated_at_risk(account, sym),
                    MarginMode::Cross => cross_risk,
                };
                if at_risk {
                    assert_eq!(band, ALWAYS, "risk index missed account {account} in symbol {sym}");
                } else if band != ALWAYS {
                    assert!(
                        band.0 <= m.mark_price && m.mark_price <= band.1,
                        "mark of {sym} outside the band of {account} without a check"
                    );
                }
            }
        }
    }

    fn isolated_at_risk(&self, account: AccountId, symbol: SymbolId) -> bool {
        let m = &self.markets[symbol as usize];
        self.position(account, symbol).is_some_and(|p| {
            !p.size.is_zero()
                && p.isolated_margin + p.unrealized_pnl(m.mark_price) < p.maintenance_margin(m.mark_price, &m.spec)
        })
    }

    fn cross_at_risk(&self, account: AccountId) -> bool {
        self.margin(account)
            .is_some_and(|s| s.cross_maintenance_margin.is_pos() && s.equity < s.cross_maintenance_margin)
    }

    fn liquidate_isolated(&mut self, account: AccountId, symbol: SymbolId) {
        self.cancel_account_orders(account, Some(symbol), DoneReason::Liquidation);
        let m = &self.markets[symbol as usize];
        let (tick, mark, liq_fee) = (m.spec.tick_size, m.mark_price, m.spec.liquidation_fee);
        let pos = self.position(account, symbol).expect("position");
        if pos.size.is_zero() {
            return;
        }
        let bp = bankruptcy_price(pos.entry_price, pos.size, pos.isolated_margin, tick);
        let fee = pos.notional(mark).mul_rate(liq_fee, Round::Up);
        let before = self.accounts[&account].balance;
        self.liquidate_position(account, symbol, bp);
        // Collateral returned to the wallet pays the clearance fee.
        let returned = self.accounts[&account].balance - before;
        self.charge_clearance(account, fee.min(returned));
    }

    fn liquidate_cross(&mut self, account: AccountId) {
        let cross_symbols: Vec<SymbolId> = self.accounts[&account]
            .positions
            .iter()
            .filter(|(_, p)| p.margin_mode == MarginMode::Cross)
            .map(|(s, _)| *s)
            .collect();
        for &s in &cross_symbols {
            self.cancel_account_orders(account, Some(s), DoneReason::Liquidation);
        }
        for _ in 0..MAX_CROSS_LIQUIDATIONS {
            if !self.cross_at_risk(account) {
                break;
            }
            let acct = &self.accounts[&account];
            // Largest maintenance requirement goes first.
            let pick = acct
                .positions
                .iter()
                .filter(|(_, p)| p.margin_mode == MarginMode::Cross && !p.size.is_zero())
                .max_by_key(|(s, p)| {
                    let m = &self.markets[**s as usize];
                    p.maintenance_margin(m.mark_price, &m.spec)
                })
                .map(|(s, _)| *s);
            let Some(symbol) = pick else { break };
            let m = &self.markets[symbol as usize];
            let pos = &acct.positions[&symbol];
            let summary = margin_summary(acct, &self.markets);
            let backing = summary.equity - pos.unrealized_pnl(m.mark_price);
            let bp = bankruptcy_price(pos.entry_price, pos.size, backing, m.spec.tick_size);
            let fee = pos.notional(m.mark_price).mul_rate(m.spec.liquidation_fee, Round::Up);
            self.liquidate_position(account, symbol, bp);
            let equity = self.margin(account).expect("account").equity;
            self.charge_clearance(account, fee.min(equity));
        }
        // Bankrupt account with nothing left to liquidate: insurance covers.
        let acct = self.accounts.get_mut(&account).expect("account");
        let flat = acct.positions.values().all(|p| p.margin_mode == MarginMode::Isolated || p.size.is_zero());
        if flat && acct.balance.is_neg() {
            let deficit = -acct.balance;
            acct.balance = Amount::ZERO;
            self.touch_risk(account);
            self.change_insurance(-deficit);
        }
    }

    fn charge_clearance(&mut self, account: AccountId, fee: Amount) {
        if fee.is_pos() {
            self.accounts.get_mut(&account).expect("account").balance -= fee;
            self.touch_risk(account);
            self.change_insurance(fee);
        }
    }

    /// Closes a whole position: IOC order at the bankruptcy price into the
    /// book, then auto-deleverages whatever the book could not absorb.
    fn liquidate_position(&mut self, account: AccountId, symbol: SymbolId, bankruptcy_price: Price) {
        let size = self.position(account, symbol).map_or(Qty::ZERO, |p| p.size);
        if size.is_zero() {
            return;
        }
        let side = if size.is_pos() { Side::Sell } else { Side::Buy };
        self.events.push(Event::Liquidation {
            account,
            symbol,
            side,
            qty: size.abs(),
            bankruptcy_price,
            mark_price: self.markets[symbol as usize].mark_price,
        });
        let id = self.alloc_order_id();
        let mut o = Order::blank(id, account, symbol, side, OrderType::Limit, size.abs(), bankruptcy_price);
        o.tif = TimeInForce::Ioc;
        o.reduce_only = true;
        o.is_liquidation = true;
        o.stp = StpMode::None;
        o.origin = OrderOrigin::Liquidation;
        let _ = self.submit_active(o, true);

        let rest = self.position(account, symbol).map_or(Qty::ZERO, |p| p.size.abs());
        if rest.is_pos() {
            self.auto_deleverage(account, symbol, side, rest, bankruptcy_price);
        }
    }

    /// Closes `qty` of the liquidated position against the most profitable,
    /// most leveraged opposite positions at the bankruptcy price.
    fn auto_deleverage(&mut self, liquidated: AccountId, symbol: SymbolId, side: Side, qty: Qty, price: Price) {
        debug_assert!(self.liq_pass.is_some(), "ADL outside a liquidation pass");
        let si = symbol as usize;
        if self.adl_rankings[si].is_none() {
            let m = &self.markets[si];
            let accounts = &self.accounts;
            let positions = m.holders.iter().map(|&a| (a, &accounts[&a].positions[&symbol]));
            self.adl_rankings[si] = Some(AdlRanking::build(positions, m.mark_price));
        }
        // Counterparties hold the opposite side, i.e. they trade `side.opposite()`
        // to close, which means their size has the sign of `side`. Take the
        // ranked prefix that covers `qty`, with sizes as of now.
        let mut ranked: Vec<(AccountId, Qty)> = Vec::new();
        let mut covered = Qty::ZERO;
        for a in self.adl_rankings[si].as_ref().expect("ranking").iter(side.sign()) {
            if covered >= qty {
                break;
            }
            if a == liquidated {
                continue;
            }
            let size = self.accounts[&a].positions[&symbol].size.abs();
            covered += size;
            ranked.push((a, size));
        }

        let mut remaining = qty;
        for (counterparty, size) in ranked {
            if !remaining.is_pos() {
                break;
            }
            let q = remaining.min(size);
            self.apply_fill(liquidated, symbol, side, price, q, Amount::ZERO);
            self.apply_fill(counterparty, symbol, side.opposite(), price, q, Amount::ZERO);
            self.events.push(Event::AutoDeleverage {
                symbol,
                liquidated,
                counterparty,
                counterparty_side: side.opposite(),
                qty: q,
                price,
            });
            remaining -= q;
        }
        self.markets[symbol as usize].last_price = price;
        self.mark_dirty(symbol);
    }
}
