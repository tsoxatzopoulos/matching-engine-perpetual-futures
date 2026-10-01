# Liquidation candidate index

## Problem

Originally, `check_liquidations(sym)` walked **every** holder of `sym` on every
`MarkPrice` and `Funding`, and recomputed the full `margin_summary` of each
cross account: O(holders × positions). It also called `settle()` after every
holder. On top of that, ADL ranked *all* holders of the symbol for *every*
liquidation, which costs O(liquidations × holders log holders).

The baseline measured, at 50k accounts × 10 symbols:
- about 10 ms for a `MarkPrice` that liquidates nobody;
- 6.4 s for one that liquidates 1,466 positions.

## Goal

The cost of a `MarkPrice` should depend on the accounts **near** liquidation,
not on the total number of accounts. The event stream must stay identical to
the reference (golden test).

## Risk bands

For each account with an open position we keep a **price band per symbol**,
`[lo_s, hi_s]`. The guarantee:

> As long as the account's state does not change and every mark stays inside
> its band, the account cannot be below maintenance margin.

Each market keeps two ordered sets, `(lo, account)` and `(hi, account)`. When
the mark of `s` moves to `m`, the candidates are exactly the accounts with
`lo_s > m` or `hi_s < m`: two range scans, the same technique as the
conditional book. An account that is already at risk (or too close to it)
gets the band `ALWAYS = (+∞, −∞)` and is a candidate on every mark of every
symbol it holds.

A band is **re-armed** (recomputed from the exact state at the current marks)
whenever:
- the account's state changes: fill, deposit or withdrawal, isolated-margin
  change, leverage top-up, funding, clearance fee, or deficit coverage;
- the account was a candidate and survived the exact check.

Dirty accounts are re-armed at the end of each liquidation pass. Until then
they are added as candidates to any pass of a symbol they hold.

## Buffer

The buffer is `B = equity − MM`. The engine liquidates exactly when `B < 0`.

- **Isolated position:** `B = isolated_margin + upnl − MM`. Its headroom is per position.
- **Cross account:** `B = wallet + Σ upnl_s − Σ MM_s` over its cross positions.

## Bound on how fast the buffer can fall

Take one position with signed size `q` and mark moving from `m₀` to `m`. Write
`n = |q|·m` for the notional and `μ` for the largest maintenance rate of the
tiers that notionals in `[0, 2·n̂]` can reach, where `n̂ ≥ |q|·m₀` is the
notional rounded up. Bands are capped at `d ≤ m₀`, so inside a band
`m ≤ 2·m₀` and the notional never leaves `[0, 2·n̂]`. For BTC-like tiers and
positions under 25k USDT that gives μ = 0.4% instead of the 50% of the 1x tier.

**The PnL change** is exactly `ΔU = q·(m − m₀)`, so `|ΔU| = |q|·|Δm|`.

**The MM change.** As a function of notional, `MM(n) = max(n·mmr(n) − cum(n), 0)`.
The maintenance amounts `cum` are built (`SymbolSpec::with_risk_tiers`) so that
`n·mmr − cum` is **continuous** at the tier boundaries. On an interval, a continuous piecewise-linear function whose slopes there are
all ≤ μ is μ-Lipschitz, and taking `max(·, 0)` keeps it μ-Lipschitz. Therefore:

    |ΔMM| ≤ μ·|Δn| = μ·|q|·|Δm|

**The total change** is `ΔB = ΔU − ΔMM`, therefore

    ΔB ≥ −(1 + μ)·|q|·|Δm|

For a cross account the symbols add up, because B is a sum over positions:

    B(m) ≥ B(m₀) − Σ_s (1 + μ_s)·|q_s|·|m_s − m₀_s|          (1)

Note that (1) holds whatever mix of symbols moved, in either direction.

## Choosing the band widths

Let:
- `slack` = the rounding allowance (next section);
- `budget = B(m₀) − slack`;
- `H = ⌊budget / 8⌋` = the **headroom**, held back for funding debits;
- `b = budget − H` = the part spent on bands;
- `W = Σ_s (1 + μ_s)·n̂_s`.

If `budget ≤ 0`, every position of the account gets `ALWAYS`. Otherwise
symbol `s` gets

    d_s = min( ⌊ b · m₀_s / W ⌋, m₀_s ),     band_s = [m₀_s − d_s, m₀_s + d_s]

Let `D` be the net debits booked against the headroom since arming. Funding
payments count positively, receipts negatively. The engine keeps `H − D` per
account (cross) or per position (isolated), and marks the account dirty as
soon as it goes negative.

**Claim.** If `|m_s − m₀_s| ≤ d_s` for every symbol `s` and `D ≤ H`, then
`B(m) ≥ slack ≥ 0`, so the account is not at risk.

**Proof.** By (1), with the debits on top:

    B(m) ≥ B(m₀) − D − Σ_s (1+μ_s)·|q_s|·d_s
         ≥ B(m₀) − D − Σ_s (1+μ_s)·|q_s|·b·m₀_s / W
         = B(m₀) − D − b · [Σ_s (1+μ_s)·|q_s|·m₀_s] / W
         ≥ B(m₀) − D − b                           (since n̂_s ≥ |q_s|·m₀_s, the bracket ≤ W)
         ≥ B(m₀) − H − b = B(m₀) − budget = slack                     ∎

This is why `Funding` no longer marks every holder dirty: a payment only
consumes headroom, and only accounts that exhaust it are re-checked.

Every rounding goes the safe way: `d` is floored, `n̂` and `W` are rounded up.
The bound is **conservative**:
- An account may be checked without being at risk (the factor `1 + μ` and the
  even split of the budget make the bands narrower than necessary). That is
  only wasted work.
- An account at risk can never be outside the candidates.

Isolated positions are the single-symbol case, with `B = isolated_margin + upnl − MM`.

## Rounding slack

The engine computes B in fixed point, not in real numbers. Per position, the
computed value differs from the real one by fewer than:
- 1 raw unit for the PnL floor;
- 2 for the notional floor times mmr plus the ceiling of MM;
- `tiers` for the per-tier flooring of the maintenance amounts (and the tier
  choice at a boundary).

The total is under `4 + 2·tiers` raw units per position, at each of the two
evaluation times. The slack used is `32 + Σ_positions (16 + 8·tiers)` raw
units. That is above twice the error, and still below 10⁻⁵ USDT for 10 tiers.

## Identical event stream

The old loop visited holders of `sym` in `AccountId` order, ran the per-holder
check *at that holder's turn*, and called `settle()` after each holder.

The new loop:
1. **Initial candidates.** Starts from a `BTreeSet` with the band breaches plus
   the dirty accounts, keeping only holders of `sym` at the start.
2. **Ordered visits.** Pops candidates in `AccountId` order and runs exactly
   the old per-holder body followed by `settle()`.
3. **Touched accounts.** Tracks every account a fill touches during the pass. If
   that account was a holder of `sym` at the start and its id is greater than
   the current cursor, it joins the candidates, because its state may now
   differ from what the band assumed.

**Why this is equivalent.** An untouched, non-dirty holder has the same state
at its turn as at the start, and the marks are fixed during the pass. By the
claim, if it is not a candidate it is not at risk, so the old body would have
done nothing for it. `settle()` after a no-op body is itself a no-op, so
skipping those holders changes no event.

## ADL ranking

The ranking key is `(score desc, AccountId asc)`, where the score depends only
on `(entry, size, leverage, mark)`. During a pass the marks are fixed, so the
ranking of a symbol is built once, on the first ADL of the pass, as an ordered
set. `apply_fill` updates the entry of any account whose position in that
symbol changes. Each ADL walks the set from the top and visits the same
prefix the old full sort produced. The cache is dropped at the end of the pass.

## Oracle

In debug builds, or with the `risk-oracle` feature, every pass ends with a full
scan. For every account and every non-zero position it asserts:
- the position has a band, and the band is present in the market's index;
- if the position's check (isolated, or cross for the account) says "at risk",
  the band is `ALWAYS`;
- otherwise the current mark lies inside the band.

The golden test and the whole test suite run with this oracle enabled in debug.
