//! Deterministic pseudo-random workloads for the golden test and benchmarks.
//!
//! Nothing here is engine logic: the generators only build `Command`s. They may
//! read engine state (open orders, positions, prices) to choose sensible
//! commands, which keeps them deterministic because the engine is.

use crate::engine::{Command, Engine};
use crate::fixed::{Amount, Price, Qty, Rate, Round};
use crate::market::SymbolSpec;
use crate::order::{NewOrder, TpSl, TrailingOffset};
use crate::types::*;

/// xorshift64*: small, fast and identical on every platform.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in `0..n` (n > 0).
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }

    /// Uniform in `lo..=hi`.
    pub fn range(&mut self, lo: i64, hi: i64) -> i64 {
        lo + self.below((hi - lo + 1) as u64) as i64
    }

    /// True with probability `pct`%.
    pub fn chance(&mut self, pct: u64) -> bool {
        self.below(100) < pct
    }

    pub fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[self.below(items.len() as u64) as usize]
    }
}

/// FNV-1a 64: a stable hash whose output never changes between Rust releases
/// (unlike `DefaultHasher`), so it can be stored in a golden file.
#[derive(Clone, Copy, Debug)]
pub struct Fnv64(u64);

impl Default for Fnv64 {
    fn default() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
}

impl Fnv64 {
    pub fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= b as u64;
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    pub fn finish(self) -> u64 {
        self.0
    }
}

/// `base * (1 + bps / 10_000)` rounded to `tick` (at least one tick).
pub fn offset_price(base: Price, bps: i64, tick: Price) -> Price {
    let p = base + Price(crate::fixed::mul_div(base.0, bps, 10_000, Round::Down));
    p.round_to(tick, Round::Down).max(tick)
}

struct SimSymbol {
    id: SymbolId,
    initial: Price,
    fair: Price,
    tick: Price,
    lot: Qty,
    max_lots: i64,
}

/// Mixed workload for the determinism golden test: three symbols, cross and
/// isolated accounts with leverage from 2x to 125x, every order type, frequent
/// mark-price moves with occasional jumps (liquidations, ADL) and funding.
pub struct GoldenWorkload {
    rng: Rng,
    accounts: u64,
    symbols: Vec<SimSymbol>,
}

impl GoldenWorkload {
    pub fn new(seed: u64, accounts: u64) -> Self {
        let mk = |id, initial: &str, tick: &str, lot: &str, max_lots| SimSymbol {
            id,
            initial: Price::parse(initial),
            fair: Price::parse(initial),
            tick: Price::parse(tick),
            lot: Qty::parse(lot),
            max_lots,
        };
        Self {
            rng: Rng::new(seed),
            accounts,
            symbols: vec![
                mk(0, "60000", "0.1", "0.001", 50),
                mk(1, "3000", "0.01", "0.01", 50),
                mk(2, "150", "0.001", "0.1", 100),
            ],
        }
    }

    /// Markets, deposits, margin modes and leverage.
    pub fn setup(&mut self) -> Vec<Command> {
        let mut cmds = Vec::new();
        for (s, name) in self.symbols.iter().zip(["BTCUSDT", "ETHUSDT", "SOLUSDT"]) {
            cmds.push(Command::AddMarket { spec: SymbolSpec::new(name, s.tick, s.lot), price: s.initial });
        }
        cmds.push(Command::FundInsurance { amount: Amount::from_int(1_000_000) });
        let n_symbols = self.symbols.len() as u64;
        for account in 1..=self.accounts {
            let amount = Amount::from_int(self.rng.range(500, 20_000));
            cmds.push(Command::Deposit { account, amount });
            if account % 10 == 0 {
                let maker = Rate::parse(if self.rng.chance(50) { "-0.0001" } else { "0.0001" });
                cmds.push(Command::SetFeeRates { account, maker: Some(maker), taker: Some(Rate::parse("0.0004")) });
            }
            for symbol in 0..n_symbols as SymbolId {
                if self.rng.chance(30) {
                    cmds.push(Command::SetMarginMode { account, symbol, mode: MarginMode::Isolated });
                }
                let leverage = self.rng.pick(&[2, 5, 10, 20, 25, 50, 75, 100, 125]);
                cmds.push(Command::SetLeverage { account, symbol, leverage });
            }
        }
        cmds
    }

    /// Next command, chosen from the current engine state.
    pub fn next(&mut self, e: &Engine) -> Command {
        let account = 1 + self.rng.below(self.accounts);
        let si = self.rng.below(self.symbols.len() as u64) as usize;
        let symbol = self.symbols[si].id;
        let side = if self.rng.chance(50) { Side::Buy } else { Side::Sell };
        match self.rng.below(1000) {
            0..=149 => self.mark_price(si),
            150..=151 => {
                let rate = Rate::parse(self.rng.pick(&["-0.0003", "-0.0001", "0.0001", "0.0003", "0.001"]));
                Command::Funding { symbol, rate }
            }
            152..=171 => Command::Deposit { account, amount: Amount::from_int(self.rng.range(100, 5_000)) },
            172..=176 => Command::Withdraw { account, amount: Amount::from_int(self.rng.range(10, 1_000)) },
            177..=186 => {
                let leverage = self.rng.pick(&[1, 3, 10, 20, 50, 100, 125]);
                Command::SetLeverage { account, symbol, leverage }
            }
            187..=196 => {
                let delta = Amount::from_int(self.rng.range(-200, 300));
                Command::AdjustIsolatedMargin { account, symbol, delta }
            }
            197..=216 => self.position_tpsl(e, account, si),
            217..=366 => self.cancel(e, account, symbol),
            367..=416 => self.amend(e, account, si),
            417..=476 => Command::PlaceOrder(self.conditional(e, account, si, side)),
            477..=516 => {
                let o = self.limit(account, si, side);
                let s = &self.symbols[si];
                let (up, down) = (self.rng.range(100, 600), -self.rng.range(100, 600));
                let (tp, sl) = match side {
                    Side::Buy => (up, down),
                    Side::Sell => (down, up),
                };
                let tp = TpSl::market(offset_price(s.fair, tp, s.tick));
                let sl = TpSl::market(offset_price(s.fair, sl, s.tick))
                    .by(if self.rng.chance(50) { TriggerBy::MarkPrice } else { TriggerBy::LastPrice });
                Command::PlaceOrder(o.tif(TimeInForce::Gtc).take_profit(tp).stop_loss(sl))
            }
            517..=616 => {
                let mut o = NewOrder::market(account, symbol, side, self.qty(si));
                if self.rng.chance(15) {
                    o = o.reduce_only();
                }
                Command::PlaceOrder(o)
            }
            _ => Command::PlaceOrder(self.limit(account, si, side)),
        }
    }

    fn qty(&mut self, si: usize) -> Qty {
        let s = &self.symbols[si];
        let lots = self.rng.range(1, s.max_lots);
        Qty(s.lot.0 * lots)
    }

    /// Random walk with occasional 4-8% jumps, kept within 0.4x..2.5x of the start.
    fn mark_price(&mut self, si: usize) -> Command {
        let jump = self.rng.chance(3);
        let bps = if jump { self.rng.range(400, 800) } else { self.rng.range(0, 30) };
        let s = &mut self.symbols[si];
        let down = if s.fair > Price(s.initial.0 * 5 / 2) {
            true
        } else if s.fair < Price(s.initial.0 * 2 / 5) {
            false
        } else {
            self.rng.chance(50)
        };
        s.fair = offset_price(s.fair, if down { -bps } else { bps }, s.tick);
        Command::MarkPrice { symbol: s.id, mark: s.fair, index: s.fair }
    }

    fn limit(&mut self, account: AccountId, si: usize, side: Side) -> NewOrder {
        let qty = self.qty(si);
        // Mostly passive, sometimes crossing the fair price.
        let bps = match side {
            Side::Buy => self.rng.range(-100, 30),
            Side::Sell => self.rng.range(-30, 100),
        };
        let s = &self.symbols[si];
        let price = offset_price(s.fair, bps, s.tick);
        let lot = s.lot;
        let mut o = NewOrder::limit(account, s.id, side, price, qty);
        o = match self.rng.below(100) {
            0..=69 => o,
            70..=79 => o.post_only(),
            80..=89 => o.ioc(),
            90..=94 => o.fok(),
            _ if qty.0 >= 3 * lot.0 => o.iceberg(Qty(qty.0 / 3 / lot.0 * lot.0)),
            _ => o,
        };
        if self.rng.chance(10) {
            o = o.reduce_only();
        }
        if self.rng.chance(10) {
            o = o.stp(self.rng.pick(&[StpMode::None, StpMode::CancelTaker, StpMode::CancelBoth]));
        }
        o.client_id(self.rng.next_u64())
    }

    fn conditional(&mut self, e: &Engine, account: AccountId, si: usize, side: Side) -> NewOrder {
        let by = if self.rng.chance(50) { TriggerBy::MarkPrice } else { TriggerBy::LastPrice };
        let s = &self.symbols[si];
        let (symbol, tick) = (s.id, s.tick);
        let current = e.market(symbol).map_or(s.fair, |m| m.price_for(by));
        let qty = self.qty(si);
        let dist = self.rng.range(50, 400);
        let above = offset_price(current, dist, tick);
        let below = offset_price(current, -dist, tick);
        let kind = self.rng.below(6);
        let mut o = match (kind, side) {
            (0, Side::Buy) => NewOrder::stop_market(account, symbol, side, above, qty),
            (0, Side::Sell) => NewOrder::stop_market(account, symbol, side, below, qty),
            (1, Side::Buy) => NewOrder::stop_limit(account, symbol, side, above, offset_price(above, 20, tick), qty),
            (1, Side::Sell) => NewOrder::stop_limit(account, symbol, side, below, offset_price(below, -20, tick), qty),
            (2, Side::Buy) => NewOrder::take_profit_market(account, symbol, side, below, qty),
            (2, Side::Sell) => NewOrder::take_profit_market(account, symbol, side, above, qty),
            (3, Side::Buy) => NewOrder::take_profit_limit(account, symbol, side, below, below, qty),
            (3, Side::Sell) => NewOrder::take_profit_limit(account, symbol, side, above, above, qty),
            (4, _) => {
                let offset = TrailingOffset::Absolute(Price(current.0 / 100 / tick.0 * tick.0).max(tick));
                NewOrder::trailing_stop(account, symbol, side, offset, None, qty)
            }
            _ => {
                let activation = if side == Side::Sell { above } else { below };
                let offset = TrailingOffset::Rate(Rate::parse("0.005"));
                NewOrder::trailing_stop(account, symbol, side, offset, Some(activation), qty)
            }
        }
        .trigger_by(by);
        if self.rng.chance(25) {
            o = if kind == 0 || kind == 2 || kind == 4 { o.close_position() } else { o.reduce_only() };
        }
        o
    }

    fn position_tpsl(&mut self, e: &Engine, account: AccountId, si: usize) -> Command {
        let s = &self.symbols[si];
        let (symbol, tick, fair) = (s.id, s.tick, s.fair);
        let long = e.position(account, symbol).is_none_or(|p| !p.size.is_neg());
        let (up, down) = (self.rng.range(100, 800), -self.rng.range(100, 800));
        let (tp, sl) = if long { (up, down) } else { (down, up) };
        let take_profit = self.rng.chance(80).then(|| TpSl::market(offset_price(fair, tp, tick)));
        let stop_loss = self.rng.chance(80).then(|| {
            let t = offset_price(fair, sl, tick);
            if self.rng.chance(30) { TpSl::limit(t, t) } else { TpSl::market(t) }.by(TriggerBy::MarkPrice)
        });
        Command::SetPositionTpSl { account, symbol, take_profit, stop_loss }
    }

    fn cancel(&mut self, e: &Engine, account: AccountId, symbol: SymbolId) -> Command {
        if self.rng.chance(3) {
            let symbol = self.rng.chance(50).then_some(symbol);
            return Command::CancelAll { account, symbol };
        }
        let open = e.open_orders(account, symbol);
        let order_id = if open.is_empty() {
            self.rng.range(1, 1_000_000) as OrderId
        } else {
            open[self.rng.below(open.len() as u64) as usize].id
        };
        Command::CancelOrder { account, symbol, order_id }
    }

    fn amend(&mut self, e: &Engine, account: AccountId, si: usize) -> Command {
        let s = &self.symbols[si];
        let (symbol, tick, lot) = (s.id, s.tick, s.lot);
        let open = e.open_orders(account, symbol);
        let Some(o) = (!open.is_empty()).then(|| &open[self.rng.below(open.len() as u64) as usize]) else {
            return Command::CancelOrder { account, symbol, order_id: 0 };
        };
        let (order_id, price, qty, trigger) = (o.id, o.price, o.qty, o.trigger.map(|t| t.price));
        let (mut new_price, mut new_qty, mut new_trigger) = (None, None, None);
        match self.rng.below(3) {
            0 if price.is_pos() => new_price = Some(offset_price(price, self.rng.range(-20, 20), tick)),
            1 if qty.is_pos() => {
                let lots = (qty.0 / lot.0 + self.rng.range(-3, 3)).max(1);
                new_qty = Some(Qty(lots * lot.0));
            }
            _ => new_trigger = trigger.filter(|t| t.is_pos()).map(|t| offset_price(t, self.rng.range(-30, 30), tick)),
        }
        Command::AmendOrder { account, symbol, order_id, price: new_price, qty: new_qty, trigger_price: new_trigger }
    }
}
