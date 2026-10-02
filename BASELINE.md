# Performance baseline

Reference numbers taken **before** any performance work. Every later change is
compared against this file, and must leave the golden event stream unchanged.

| | |
|---|---|
| Date | 2026-10-01 |
| Code | commit `d68367b` + edition 2024, `sim.rs`, golden test and `perp_bench` (no engine logic changes) |
| Machine | Apple M4, 10 cores, 16 GB, macOS (no core pinning) |
| Toolchain | rustc 1.92.0, `--release` (`lto = "fat"`, `codegen-units = 1`) |

## 1. Determinism golden test

`tests/golden.rs` replays a fixed pseudo-random workload and compares the
FNV-1a hash of every event (its `Debug` text) against `tests/golden/events.txt`.
The hash is kept per chunk of 10,000 commands, so a divergence points to where
it started.

- **Workload:** seed `0x5eed2026`, 400 accounts, 3 symbols (BTC/ETH/SOL-like), 200,000 commands.
- **Accounts:** about 30% of the account/symbol pairs are isolated, with leverage from 1x to 125x.
- **Order flow:** every order type and time in force, attached and position TP/SL, trailing stops, amends and cancels.
- **Market events:** 15% `MarkPrice` (0–0.3% steps, 3% chance of a 4–8% jump), funding, deposits, withdrawals, leverage and margin changes.

| Metric | Value |
|---|---:|
| Total hash | `d30dc1e25a672839` |
| Events | 817,727 |
| Trades | 96,923 |
| Liquidations | 9,046 |
| ADL fills | 4,329 |
| Conditional triggers | 17,105 |
| Funding payments | 128,646 |
| Order rejects | 12,467 |
| Command rejects | 30,319 |

The debug build (with overflow checks) produces the same hash as release. A
second test checks that two runs in the same process agree.

```
cargo test --release --test golden                    # verify
GOLDEN_BLESS=1 cargo test --release --test golden     # re-record (intended behaviour change only)
```

## 2. Spot-style throughput (`bench`)

60% limit, 30% cancel and 10% market orders from 1,000 accounts on one symbol,
2M commands. Three runs:

| Run | Commands/s | Mean latency |
|---:|---:|---:|
| 1 | 1,663,089 | 601 ns |
| 2 | 1,638,500 | 610 ns |
| 3 | 1,664,259 | 601 ns |

## 3. Perpetuals benchmark (`perp_bench`)

**Setup:** accounts are paired on opposite sides of every symbol.
- Positions are sized at 20–80% of what each account's margin allows.
- Leverage is drawn from {5, 10, 20, 50, 100}, and 30% of the account/symbol pairs are isolated.

**Measured run:** 100,000 order commands per configuration.
- The order mix is 55% limit within ±0.5% of mark, 30% cancel and 15% market.
- A `MarkPrice` (±0.2% step, round-robin over symbols) runs every 500 orders.
- A `Funding` runs every 20 `MarkPrice`.

```
cargo run --release --bin perp_bench
```

### Order commands (place / cancel), µs

| accounts | symbols | positions | setup s | p50 | p99 | p99.9 | max | mean |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1000 | 1 | 888 | 0.00 | 0.25 | 1.79 | 2.71 | 81.50 | 0.33 |
| 1000 | 10 | 9978 | 0.01 | 0.38 | 1.79 | 2.62 | 27.46 | 0.41 |
| 10000 | 1 | 9163 | 0.01 | 0.21 | 1.46 | 2.04 | 25.50 | 0.28 |
| 10000 | 10 | 99868 | 0.06 | 0.50 | 2.79 | 5.79 | 32.88 | 0.60 |
| 50000 | 1 | 45991 | 0.04 | 0.29 | 2.04 | 4.50 | 23.00 | 0.38 |
| 50000 | 10 | 499398 | 0.30 | 0.67 | 4.08 | 8.54 | 122.04 | 0.84 |

### MarkPrice and Funding, ms

| accounts | symbols | MarkPrice n | mean | p50 | p99 | max | Funding n | mean | max | liquidations | ADL | setup rejects |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1000 | 1 | 199 | 0.099 | 0.064 | 0.827 | 0.984 | 9 | 0.106 | 0.131 | 238 | 229 | 82 |
| 1000 | 10 | 199 | 0.198 | 0.177 | 1.051 | 1.478 | 9 | 0.205 | 0.220 | 107 | 219 | 22 |
| 10000 | 1 | 199 | 1.262 | 0.555 | 27.811 | 31.984 | 9 | 0.891 | 1.159 | 963 | 1727 | 778 |
| 10000 | 10 | 199 | 4.168 | 1.836 | 97.041 | 111.880 | 9 | 2.193 | 2.294 | 1334 | 2895 | 132 |
| 50000 | 1 | 199 | 57.592 | 5.778 | 1952.954 | 2201.561 | 9 | 16.567 | 60.398 | 5480 | 12261 | 3993 |
| 50000 | 10 | 199 | 130.923 | 9.812 | 5628.398 | 6355.298 | 9 | 12.345 | 13.395 | 7488 | 15397 | 602 |

### MarkPrice split: without vs with liquidations, ms

| accounts | symbols | quiet n | quiet p50 | quiet max | liquidating n | liq p50 | liq max | max liquidations in one MarkPrice |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1000 | 1 | 164 | 0.062 | 0.097 | 35 | 0.199 | 0.984 | 38 |
| 1000 | 10 | 186 | 0.176 | 0.236 | 13 | 0.344 | 1.478 | 33 |
| 10000 | 1 | 155 | 0.553 | 0.894 | 44 | 0.585 | 31.984 | 107 |
| 10000 | 10 | 184 | 1.830 | 2.282 | 15 | 11.897 | 111.880 | 306 |
| 50000 | 1 | 136 | 5.641 | 8.560 | 63 | 19.500 | 2201.561 | 1057 |
| 50000 | 10 | 185 | 9.798 | 11.304 | 14 | 418.942 | 6355.298 | 1466 |

## Findings

1. **The risk scan grows with the number of holders.** A `MarkPrice` that liquidates nobody still costs about 9.8 ms at 50k accounts × 10 symbols. It walks every holder of the symbol and recomputes the full `margin_summary` of every cross account.
2. **Liquidation cascades are the real problem.** One `MarkPrice` that liquidates 1,466 positions blocks the engine for **6.4 s**, about 4 ms per liquidation. Each liquidation calls ADL, which ranks *all* holders of the symbol, and `check_liquidations` runs `settle()` after every holder. The cost is O(liquidations × holders), and it lands exactly when the market is most violent.
3. **Order latency is fine and barely depends on account count.** p50 is 0.2–0.7 µs and p99.9 under 10 µs. The max outliers (up to 122 µs) are single events, most likely allocation or OS noise.
4. **Funding** costs one pass over holders, about 12–17 ms at 50k accounts.

## Caveats

- Each configuration is a single run. Means vary by roughly ±5% between runs; max values vary much more.
- `Instant::now()` adds tens of nanoseconds to each order measurement.
- The perp benchmark is a stress scenario: positions near 100x with 80% sizing are liquidated by ±0.2% moves. The *ratio* between configurations matters more than the absolute values.

---

# Results after the performance work (phases 3–6)

Same machine and toolchain. The golden event stream is **unchanged**
(`d30dc1e25a672839`, 817,727 events). The full-scan liquidation oracle runs after
every `MarkPrice` and `Funding` in debug builds, and in release with
`--features risk-oracle`. It has never fired.

| Phase | Change | Kept | Effect |
|---|---|---|---|
| 3 | Liquidation candidate index (`src/risk.rs`, proof in `docs/liquidation-index.md`), per-pass ADL ranking, funding headroom | yes | MarkPrice cost follows accounts *near* liquidation |
| 4 | Book node split: 64-byte hot `Resting` + cold `OrderMeta` (was 296 bytes per node) | yes | no measurable change on these workloads (±1%) |
| 5a | FxHash-style hasher instead of SipHash | yes | +9% order throughput |
| 5b | `Engine::apply` returns events from a reused buffer | yes | +17% |
| 5c | Reuse of the `touched` buffer in `settle()` | yes | +2.5% |
| 5d | Reused buffers in the liquidation pass and ADL | **no** | within noise, reverted |
| 6 | Lock-free SPSC rings in the runtime (`src/spsc.rs`) | yes | not benchmarked (no runtime benchmark yet) |

## Order throughput (`bench`, 2M commands)

The original `bench` parsed decimal strings (`Qty::parse`) inside the loop,
which inflated every command by ~150 ns. Section 2 above is therefore not
comparable. The original code was re-measured with the corrected benchmark
(same workload, same trades), with runs interleaved:

| Code | Commands/s (3 runs) |
|---|---|
| Original (`d68367b`), `process` | 2.09M, 2.02M, 2.03M |
| Final, `process` (Vec per command) | 2.20M, 2.22M (one outlier at 1.67M) |
| Final, `apply` (reused buffer) | **2.63M, 2.51M, 2.57M** (+25%) |

## Perpetuals benchmark (`perp_bench`)

`perp_bench` now runs one MarkPrice per symbol after setup ("warm-up") and
reports it separately: it arms every position created during setup at once.
In the baseline that cost was hidden in the first measured MarkPrice.

### Before → after, 50k accounts × 10 symbols

| Metric | Baseline | Final |
|---|---:|---:|
| MarkPrice without liquidations, p50 | 9.80 ms | **1.22 ms** |
| MarkPrice without liquidations, max | 11.30 ms | 8.76 ms |
| MarkPrice with liquidations, p50 | 418.9 ms | **31.8 ms** |
| MarkPrice with liquidations, max (1,466 liquidations) | 6,355 ms | **44 ms** |
| MarkPrice mean | 130.9 ms | 3.2 ms |
| Funding mean | 12.3 ms | 8.6 ms |
| Order p50 / p99.9 | 0.67 / 8.54 µs | 0.67 / 6.42 µs |
| Liquidations / ADL fills | 7,488 / 15,397 | 7,488 / 15,397 (identical) |
| One-off warm-up (arming 499k positions) | n/a | 147 ms |

### Final tables

#### Order commands (place / cancel), µs

| accounts | symbols | positions | setup s | warm-up ms | p50 | p99 | p99.9 | max | mean |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1000 | 1 | 888 | 0.00 | 1.1 | 0.25 | 1.96 | 3.33 | 72.92 | 0.35 |
| 1000 | 10 | 9978 | 0.01 | 1.8 | 0.38 | 1.54 | 2.62 | 23.54 | 0.38 |
| 10000 | 1 | 9163 | 0.01 | 3.4 | 0.17 | 1.17 | 1.62 | 34.21 | 0.23 |
| 10000 | 10 | 99868 | 0.05 | 21.3 | 0.50 | 2.29 | 4.88 | 30.62 | 0.57 |
| 50000 | 1 | 45991 | 0.05 | 27.3 | 0.25 | 1.54 | 2.79 | 98.58 | 0.33 |
| 50000 | 10 | 499398 | 0.26 | 147.3 | 0.67 | 3.58 | 6.42 | 203.04 | 0.79 |

#### MarkPrice and Funding, ms

| accounts | symbols | MarkPrice n | mean | p50 | p99 | max | Funding n | mean | max | liquidations | ADL | setup rejects |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1000 | 1 | 199 | 0.144 | 0.097 | 0.956 | 1.109 | 9 | 0.062 | 0.245 | 238 | 229 | 82 |
| 1000 | 10 | 199 | 0.470 | 0.444 | 1.260 | 1.834 | 9 | 0.050 | 0.130 | 107 | 219 | 22 |
| 10000 | 1 | 199 | 0.211 | 0.090 | 2.478 | 2.548 | 9 | 0.305 | 0.459 | 963 | 1727 | 778 |
| 10000 | 10 | 199 | 1.018 | 0.692 | 6.041 | 6.284 | 9 | 0.918 | 1.073 | 1334 | 2895 | 132 |
| 50000 | 1 | 199 | 2.033 | 0.124 | 19.977 | 20.037 | 9 | 5.733 | 12.627 | 5480 | 12261 | 3993 |
| 50000 | 10 | 199 | 3.203 | 1.236 | 42.357 | 44.079 | 9 | 8.624 | 9.661 | 7488 | 15397 | 602 |

#### MarkPrice split, ms

| accounts | symbols | quiet n | quiet p50 | quiet max | liquidating n | liq p50 | liq max | max liquidations in one MarkPrice |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1000 | 1 | 164 | 0.087 | 0.319 | 35 | 0.323 | 1.109 | 38 |
| 1000 | 10 | 186 | 0.441 | 0.575 | 13 | 0.687 | 1.834 | 33 |
| 10000 | 1 | 155 | 0.085 | 0.177 | 44 | 0.171 | 2.548 | 107 |
| 10000 | 10 | 184 | 0.683 | 1.447 | 15 | 5.278 | 6.284 | 306 |
| 50000 | 1 | 136 | 0.115 | 0.296 | 63 | 9.341 | 20.037 | 1057 |
| 50000 | 10 | 185 | 1.217 | 8.756 | 14 | 31.810 | 44.079 | 1466 |

## What got worse

- **Small venues.** At 1k accounts × 10 symbols, a quiet MarkPrice went from
  0.18 ms to 0.44 ms. With so few accounts, a full scan is cheaper than
  re-arming the bands of every account that traded since the last mark (each
  re-arm touches 10 positions and 40 ordered-set entries). The index starts to
  win from about 10k accounts.
- **First MarkPrice after a burst of new positions.** It arms all of them at
  once: 147 ms for 499k positions in this benchmark. On a live venue, marks
  arrive continuously, so the dirty set stays small.
- **Liquidating MarkPrice with 10 symbols** still costs ~30–45 ms. The
  remaining cost is building the ADL ranking once per symbol per pass, which is
  O(holders · log holders).

---

# Follow-up after review

## Spec validation

`add_market` now calls `SymbolSpec::validate`, which checks ascending tier
limits, sane rates and maintenance-margin continuity at every tier boundary.
The liquidation proof depends on that continuity, and `risk_tiers` is a public
field. A spec that breaks it is rejected with `InvalidSpec`.

## ADL ranking: ordered set → lazy binary heap

Total time spent in ADL during the `perp_bench` run (temporary instrumentation,
since removed):

| Config | Before | After | Ranking builds |
|---|---:|---:|---:|
| 50k × 10 | 166 ms | 108 ms | 10 |
| 50k × 1 | 342 ms | 188 ms | 36 |
| 10k × 10 | 31 ms | 18 ms | 13 |

What remains (~5 ms per build at 50k holders) is looking up every position
and computing its score, not sorting. In the same runs, re-arming bands cost
more than ADL at 10 symbols (567 ms total at 50k × 10, of which 143 ms is the
one-off warm-up).

## Re-arm policy

`perp_bench --rearm=…`, 100k orders, 50k accounts × 10 symbols:

| Policy | Order mean | Order p99 / p99.9 | Order max | First mark after mass trading | Quiet MarkPrice p50 | Liquidating MarkPrice max |
|---|---:|---:|---:|---:|---:|---:|
| `OnMark` (default) | 0.80 µs | 3.5 / 6.5 µs | 109 µs | 143 ms | 1.24 ms | 35.7 ms |
| `PerCommand` | 3.75 µs | 31.9 / 45.0 µs | 158 µs | 0 | ~0 | 40.5 ms |
| `Threshold(1000)` | 0.79 µs | 3.5 / 6.5 µs | 169 µs | 3.0 ms | 1.27 ms | 39.0 ms |
| `Threshold(256)` | 0.83 µs | 3.5 / 6.9 µs | 1,672 µs | 0.3 ms | 1.33 ms | 39.4 ms |

- Re-arming one account with positions in 10 symbols costs about 6.5 µs,
  mostly 40 ordered-set operations.
- `PerCommand` removes the mark-time cost, but makes every trading order pay:
  mean ×4.7, p99 ×9.
- `Threshold(n)` keeps orders cheap and bounds what a mark inherits. The
  spike moves to the one order that crosses the threshold, about n × 6.5 µs.
- Idle-time re-arming in the threaded runtime spreads the work without
  charging any order, but a burst leaves no idle time. Not measured:
  `perp_bench` drives the engine directly, without the runtime.

## Wait strategy

Engine thread idle for 2 s: `Spin` used 1.98 s of CPU, `Backoff` 0.10 s.
