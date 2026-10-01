//! Liquidation candidate index and ADL ranking.
//!
//! See `docs/liquidation-index.md` for the proof that the bands are
//! conservative: an account whose marks all stay inside its bands cannot be
//! below maintenance margin, so only band breaches need an exact check.

use std::cmp::Reverse;
use std::collections::BTreeSet;

use crate::account::{Account, Position};
use crate::fixed::{Amount, Price, SCALE};
use crate::hash::FastMap;
use crate::market::{Market, SymbolSpec};
use crate::types::{AccountId, MarginMode, SymbolId};

/// `(lo, hi)`: the account is a candidate when the mark leaves `[lo, hi]`.
pub type Band = (Price, Price);

/// Band of an account that is at risk (or too close to tell): a candidate on
/// every mark of every symbol it holds.
pub const ALWAYS: Band = (Price(i64::MAX), Price(i64::MIN));

/// Per-symbol index of risk bands.
#[derive(Default)]
pub struct RiskBands {
    lower: BTreeSet<(Price, AccountId)>,
    upper: BTreeSet<(Price, AccountId)>,
}

impl RiskBands {
    pub fn insert(&mut self, account: AccountId, band: Band) {
        self.lower.insert((band.0, account));
        self.upper.insert((band.1, account));
    }

    pub fn remove(&mut self, account: AccountId, band: Band) {
        self.lower.remove(&(band.0, account));
        self.upper.remove(&(band.1, account));
    }

    pub fn contains(&self, account: AccountId, band: Band) -> bool {
        self.lower.contains(&(band.0, account)) && self.upper.contains(&(band.1, account))
    }

    pub fn len(&self) -> usize {
        self.lower.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lower.is_empty()
    }

    /// Accounts whose band does not contain `mark`.
    pub fn collect(&self, mark: Price, out: &mut BTreeSet<AccountId>) {
        out.extend(self.lower.range((Price(mark.0.saturating_add(1)), 0)..).map(|&(_, a)| a));
        out.extend(self.upper.range(..(mark, 0)).map(|&(_, a)| a));
    }
}

/// Largest maintenance rate over the tiers that notionals in `[0, n_hi]`
/// can reach: the Lipschitz constant of MM on that interval.
fn local_max_mmr_raw(spec: &SymbolSpec, n_hi: i128) -> i128 {
    let mut lower = 0i128;
    let mut mu = 0i128;
    for t in &spec.risk_tiers {
        if lower > n_hi {
            break;
        }
        mu = mu.max(t.mmr.0 as i128);
        lower = t.max_notional.0 as i128;
    }
    if lower <= n_hi {
        // Beyond the last tier the last rate applies.
        mu = mu.max(spec.risk_tiers.last().map_or(0, |t| t.mmr.0 as i128));
    }
    mu
}

/// Rounding allowance per position, in raw units (see the doc).
fn slack_raw(spec: &SymbolSpec) -> i128 {
    16 + 8 * spec.risk_tiers.len() as i128
}

const BASE_SLACK: i128 = 32;

/// Sensitivity weight `(1 + μ)·n̂`, rounded up, in raw amount units. Bands
/// never exceed `±mark`, so the notional stays within `[0, 2·n̂]` and μ only
/// needs to cover the tiers in that range.
fn weight(pos: &Position, mark: Price, spec: &SymbolSpec) -> i128 {
    let n_up = pos.notional(mark).0 as i128 + 1;
    let s = SCALE as i128;
    (n_up * (s + local_max_mmr_raw(spec, 2 * n_up)) + s - 1) / s
}

fn band(mark: Price, budget: i128, w: i128) -> Band {
    if budget <= 0 || w <= 0 {
        return ALWAYS;
    }
    let d = (budget * mark.0 as i128 / w).min(mark.0 as i128) as i64;
    (Price(mark.0 - d), Price(mark.0.saturating_add(d)))
}

/// Part of the buffer held back from the bands, so that small debits (funding)
/// can be absorbed without re-arming: 1/8 of the budget.
fn split_headroom(budget: i128) -> (i128, Amount) {
    if budget <= 0 {
        return (budget, Amount::ZERO);
    }
    let headroom = budget / 8;
    (budget - headroom, Amount(headroom.min(i64::MAX as i128) as i64))
}

/// Result of arming one account.
#[derive(Default)]
pub struct Armed {
    /// `(symbol, band, isolated headroom)` per non-zero position; the
    /// headroom is zero for cross positions.
    pub bands: Vec<(SymbolId, Band, Amount)>,
    /// Headroom shared by the account's cross positions.
    pub cross_headroom: Amount,
}

/// Bands for every non-zero position of `acct` at the current marks.
pub fn compute_bands(acct: &Account, markets: &[Market], armed: &mut Armed) {
    let out = &mut armed.bands;
    out.clear();
    let mut cross_buffer = acct.balance.0 as i128;
    let mut cross_weight = 0i128;
    let mut cross_slack = BASE_SLACK;
    let first_cross = out.len();
    for (&sym, pos) in &acct.positions {
        if pos.size.is_zero() {
            continue;
        }
        let m = &markets[sym as usize];
        let mark = m.mark_price;
        let w = weight(pos, mark, &m.spec);
        let upnl = pos.unrealized_pnl(mark).0 as i128;
        let mm = pos.maintenance_margin(mark, &m.spec).0 as i128;
        match pos.margin_mode {
            MarginMode::Isolated => {
                let buffer = pos.isolated_margin.0 as i128 + upnl - mm;
                let (budget, headroom) = split_headroom(buffer - BASE_SLACK - slack_raw(&m.spec));
                out.push((sym, band(mark, budget, w), headroom));
            }
            MarginMode::Cross => {
                cross_buffer += upnl - mm;
                cross_weight += w;
                cross_slack += slack_raw(&m.spec);
                // Placeholder, filled once the account-wide budget is known.
                out.push((sym, ALWAYS, Amount::ZERO));
            }
        }
    }
    let (budget, headroom) = split_headroom(cross_buffer - cross_slack);
    armed.cross_headroom = headroom;
    for entry in &mut out[first_cross..] {
        let pos = &acct.positions[&entry.0];
        if pos.margin_mode == MarginMode::Cross {
            entry.1 = band(markets[entry.0 as usize].mark_price, budget, cross_weight);
        }
    }
}

/// ADL priority: profitable and highly leveraged positions first.
pub fn adl_score(pos: &Position, mark: Price) -> i128 {
    let ratio = pos.pnl_ratio(mark).0 as i128;
    let lev = pos.leverage as i128;
    if ratio > 0 { ratio * lev } else { ratio / lev }
}

#[inline]
fn side_index(sign: i64) -> usize {
    if sign > 0 { 0 } else { 1 }
}

/// ADL ranking of one symbol, valid while its mark is fixed (one liquidation pass).
#[derive(Default)]
pub struct AdlRanking {
    /// `[longs, shorts]`, ordered by score descending, then account ascending.
    ranked: [BTreeSet<(Reverse<i128>, AccountId)>; 2],
    entries: FastMap<AccountId, (usize, i128)>,
}

impl AdlRanking {
    pub fn build<'a>(positions: impl Iterator<Item = (AccountId, &'a Position)>, mark: Price) -> Self {
        let mut r = Self::default();
        for (a, p) in positions {
            r.update(a, p, mark);
        }
        r
    }

    /// Re-ranks an account after its position changed.
    pub fn update(&mut self, account: AccountId, pos: &Position, mark: Price) {
        if let Some((side, score)) = self.entries.remove(&account) {
            self.ranked[side].remove(&(Reverse(score), account));
        }
        if !pos.size.is_zero() {
            let side = side_index(pos.size.signum());
            let score = adl_score(pos, mark);
            self.ranked[side].insert((Reverse(score), account));
            self.entries.insert(account, (side, score));
        }
    }

    /// Accounts holding a position of sign `sign`, best ADL candidates first.
    pub fn iter(&self, sign: i64) -> impl Iterator<Item = AccountId> + '_ {
        self.ranked[side_index(sign)].iter().map(|&(_, a)| a)
    }
}
