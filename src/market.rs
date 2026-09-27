//! Contract specification, risk tiers and per-symbol market state.

use std::collections::BTreeSet;

use crate::book::OrderBook;
use crate::conditional::ConditionalBook;
use crate::fixed::{Amount, Price, Qty, Rate, Round};
use crate::types::{AccountId, SymbolId, TriggerBy};

/// One risk-limit bracket: positions up to `max_notional` may use at most
/// `max_leverage` and need `mmr` maintenance margin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RiskTier {
    pub max_notional: Amount,
    pub max_leverage: u32,
    pub mmr: Rate,
    /// Maintenance amount deducted so that MM is continuous across tiers.
    pub maint_amount: Amount,
}

/// Static contract parameters of a linear (quote-margined) perpetual.
#[derive(Clone, Debug)]
pub struct SymbolSpec {
    pub id: SymbolId,
    pub name: String,
    pub tick_size: Price,
    pub lot_size: Qty,
    pub min_qty: Qty,
    pub max_qty: Qty,
    pub min_notional: Amount,
    pub maker_fee: Rate,
    pub taker_fee: Rate,
    /// Clearance fee on liquidated notional, paid to the insurance fund.
    pub liquidation_fee: Rate,
    /// Aggressive limit prices must stay within mark * (1 ± price_band).
    pub price_band: Rate,
    /// Market orders are protected at mark * (1 ± market_slippage).
    pub market_slippage: Rate,
    pub default_leverage: u32,
    pub risk_tiers: Vec<RiskTier>,
}

impl SymbolSpec {
    /// Spec with Binance-like defaults for BTC perpetuals.
    pub fn new(name: &str, tick_size: Price, lot_size: Qty) -> Self {
        let a = |s: &str| Amount::parse(s);
        let r = |s: &str| Rate::parse(s);
        Self {
            id: 0,
            name: name.to_string(),
            tick_size,
            lot_size,
            min_qty: lot_size,
            max_qty: Qty::from_int(1_000_000),
            min_notional: a("5"),
            maker_fee: r("0.0002"),
            taker_fee: r("0.0005"),
            liquidation_fee: r("0.0125"),
            price_band: r("0.05"),
            market_slippage: r("0.05"),
            default_leverage: 20,
            risk_tiers: Vec::new(),
        }
        .with_risk_tiers(&[
            (a("50000"), 125, r("0.004")),
            (a("250000"), 100, r("0.005")),
            (a("3000000"), 50, r("0.01")),
            (a("15000000"), 20, r("0.025")),
            (a("30000000"), 10, r("0.05")),
            (a("80000000"), 5, r("0.1")),
            (a("100000000"), 4, r("0.125")),
            (a("200000000"), 3, r("0.15")),
            (a("300000000"), 2, r("0.25")),
            (a("500000000"), 1, r("0.5")),
        ])
    }

    /// Sets the risk brackets `(max_notional, max_leverage, mmr)`, ascending,
    /// and derives each tier's maintenance amount.
    pub fn with_risk_tiers(mut self, tiers: &[(Amount, u32, Rate)]) -> Self {
        assert!(!tiers.is_empty(), "at least one risk tier required");
        let mut out: Vec<RiskTier> = Vec::with_capacity(tiers.len());
        for &(max_notional, max_leverage, mmr) in tiers {
            let maint_amount = match out.last() {
                None => Amount::ZERO,
                Some(prev) => prev.maint_amount + prev.max_notional.mul_rate(mmr - prev.mmr, Round::Down),
            };
            out.push(RiskTier { max_notional, max_leverage, mmr, maint_amount });
        }
        self.risk_tiers = out;
        self.default_leverage = self.default_leverage.min(self.max_leverage());
        self
    }

    pub fn with_fees(mut self, maker: Rate, taker: Rate) -> Self {
        self.maker_fee = maker;
        self.taker_fee = taker;
        self
    }

    pub fn with_default_leverage(mut self, leverage: u32) -> Self {
        self.default_leverage = leverage.clamp(1, self.max_leverage());
        self
    }

    pub fn with_min_notional(mut self, min_notional: Amount) -> Self {
        self.min_notional = min_notional;
        self
    }

    pub fn with_qty_limits(mut self, min_qty: Qty, max_qty: Qty) -> Self {
        self.min_qty = min_qty;
        self.max_qty = max_qty;
        self
    }

    pub fn with_price_band(mut self, band: Rate) -> Self {
        self.price_band = band;
        self
    }

    pub fn with_market_slippage(mut self, slippage: Rate) -> Self {
        self.market_slippage = slippage;
        self
    }

    pub fn with_liquidation_fee(mut self, fee: Rate) -> Self {
        self.liquidation_fee = fee;
        self
    }

    /// Tier for a position notional, `None` above the risk limit.
    pub fn tier_for(&self, notional: Amount) -> Option<&RiskTier> {
        self.risk_tiers.iter().find(|t| notional <= t.max_notional)
    }

    pub fn tier_or_last(&self, notional: Amount) -> &RiskTier {
        self.tier_for(notional).unwrap_or_else(|| self.risk_tiers.last().expect("risk tiers"))
    }

    pub fn max_leverage(&self) -> u32 {
        self.risk_tiers.first().map_or(1, |t| t.max_leverage)
    }
}

/// Live state of one symbol.
pub struct Market {
    pub spec: SymbolSpec,
    pub book: OrderBook,
    pub cond: ConditionalBook,
    pub last_price: Price,
    pub mark_price: Price,
    pub index_price: Price,
    /// Accounts with a non-zero position (deterministic order).
    pub holders: BTreeSet<AccountId>,
}

impl Market {
    pub fn new(spec: SymbolSpec, price: Price) -> Self {
        Self {
            spec,
            book: OrderBook::new(),
            cond: ConditionalBook::new(),
            last_price: price,
            mark_price: price,
            index_price: price,
            holders: BTreeSet::new(),
        }
    }

    #[inline]
    pub fn price_for(&self, by: TriggerBy) -> Price {
        match by {
            TriggerBy::LastPrice => self.last_price,
            TriggerBy::MarkPrice => self.mark_price,
        }
    }
}
