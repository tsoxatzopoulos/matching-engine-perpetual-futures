# futures-engine

A deterministic matching engine for **crypto perpetual futures**, written in Rust with zero external dependencies.

It covers the whole stack you need for a futures exchange: an order book, the full set of order types (stops, take-profit, trailing stops, TP/SL, OCO), leverage and margin, and liquidation.

```
cargo test                          # 36 tests
cargo run --example quickstart      # small end-to-end example
cargo run --release --bin bench     # throughput benchmark
```

---

## Features

### Matching
- Central limit order book with **price-time priority**
- Partial fills; the taker always trades at the maker's price
- **Time in force:** `GTC`, `IOC`, `FOK`, `GTX` (post-only)
- **Self-trade prevention:** cancel maker (default), cancel taker, cancel both, or allow
- **Iceberg orders:** only `display_qty` is shown, and each refreshed slice goes to the back of the queue
- **Amend:** reducing size at the same price keeps queue priority; changing price or increasing size re-enters the order
- Cancel a single order or all orders (per symbol or account-wide)

### Order types
| Type | Behaviour |
|---|---|
| `Limit` | Rests in the book if not fully filled |
| `Market` | Executes immediately. Slippage protection caps it at `mark ± market_slippage` |
| `StopMarket` / `StopLimit` | Breakout stop: a buy fires when price rises to the trigger, a sell when it falls |
| `TakeProfitMarket` / `TakeProfitLimit` | A buy fires when price falls to the trigger, a sell when it rises |
| `TrailingStopMarket` | Follows the best price at a fixed or percentage callback, with an optional activation price |

- Triggers can use the **last traded price** or the **mark price**.
- An order that would trigger immediately is rejected, not executed.
- **Reduce-only** orders are capped at the position size. Resting ones are cancelled automatically when the position shrinks or closes.
- **Close-position** conditional orders close whatever position exists when they fire.

### TP / SL
- **Attached TP/SL:** the TP and SL legs of an entry order become reduce-only conditional orders as the entry fills, and they grow with partial fills.
- **Position TP/SL:** these close the entire position.
- Both are **OCO-linked**: when one leg triggers, the other is cancelled.

### Margin and risk
- Linear (USDT-margined) perpetuals
- **Cross** and **isolated** margin, selectable per symbol
- **Leverage** per symbol, changeable at any time if margin allows
- Add or remove isolated collateral
- **Risk tiers:** max leverage and maintenance margin rate by position notional, with continuous maintenance amounts
- Pre-trade margin check that includes open orders. Order quantity that only reduces a position needs no margin.
- **Price band:** aggressive limit prices must stay within `mark ± price_band`
- Maker/taker fees per symbol, overridable per account (VIP tiers, negative maker rebates)

### Liquidation and funding
- Liquidation is driven by the **mark price** and uses tiered maintenance margin
- Before liquidating, all of the account's open orders are cancelled
- The position is closed with an IOC order at the **bankruptcy price**
- A **clearance fee** goes to the **insurance fund**
- **Auto-deleveraging (ADL)** absorbs what the book can't. Counterparties are ranked by PnL × leverage.
- The insurance fund covers bankrupt accounts
- **Funding:** periodic payments between longs and shorts

---

## Architecture

```
            ┌──────────────────────────────────────────┐
 Commands ─►│ EngineHandle  (sequencer thread)         │
            │   bounded channel ─► Engine::process()   │──► Vec<Event>
            └──────────────────────────────────────────┘
                              │
      ┌───────────────┬───────┴───────┬───────────────────┐
      ▼               ▼               ▼                   ▼
  Market[sym]      Accounts         Risk              Cascades
  ├ OrderBook      ├ balance        ├ margin summary   ├ stop / TP / trailing triggers
  ├ Conditional    ├ Position/sym   ├ risk tiers       ├ OCO cancellation
  │   book         │  ├ size, entry ├ liquidation      ├ reduce-only maintenance
  └ last / mark    │  └ open orders └ ADL, insurance   └ liquidation → more triggers
```

### Design decisions

**One thread for every symbol.** Spot engines often shard by symbol, one core per book. Futures can't do that easily, because cross margin couples every position of an account. A move in one market can liquidate positions in another. So the engine is a single-threaded state machine, and the runtime puts it behind a sequencer thread.

**Deterministic.** The same command sequence always produces the same events and the same state. Nothing depends on wall-clock time, iteration order is fixed (`BTreeMap` / `BTreeSet` wherever it matters), and there is no floating point. Journaling the input commands is therefore enough for recovery, replay, auditing or a hot standby.

**Fixed-point arithmetic.** All values are `i64` scaled by 10⁸, wrapped in distinct newtypes: `Price`, `Qty`, `Amount`, `Rate`. The compiler rejects mixing them up. Products go through `i128`, and every division rounds in an explicit direction.

**Order book layout.** Orders live in a slab (`Vec` + free list) and form an intrusive doubly-linked list per price level, so insert, cancel and fill are O(1) once the level is found. Levels live in a `BTreeMap`. Crypto prices span too many orders of magnitude for a flat price-indexed array.

**Conditional orders** sit outside the book, indexed by trigger price in ordered sets per price source (last or mark). A price move fires them with a range scan. Trailing stops move their trigger on every tick.

**Settle loop.** After every command, the engine runs trigger scans and reduce-only maintenance until nothing changes. A stop can fire, trade, move the price and fire another stop, all within the same command, in a deterministic order.

---

## Usage

```rust
use futures_engine::*;

fn main() {
    let mut engine = Engine::new();
    let btc = engine.add_market(
        SymbolSpec::new("BTCUSDT", Price::parse("0.1"), Qty::parse("0.001")),
        Price::parse("60000"),
    );

    engine.deposit(1, Amount::parse("10000")).unwrap();
    engine.deposit(2, Amount::parse("10000")).unwrap();
    engine.set_leverage(1, btc, 20).unwrap();

    // Maker
    engine
        .place_order(NewOrder::limit(2, btc, Side::Sell, Price::parse("60000"), Qty::parse("0.1")))
        .unwrap();

    // Taker with attached take-profit and a mark-price stop-loss
    engine
        .place_order(
            NewOrder::limit(1, btc, Side::Buy, Price::parse("60000"), Qty::parse("0.1"))
                .take_profit(TpSl::market(Price::parse("63000")))
                .stop_loss(TpSl::market(Price::parse("58000")).by(TriggerBy::MarkPrice)),
        )
        .unwrap();

    for event in engine.take_events() {
        println!("{event:?}");
    }
    println!("liquidation price: {:?}", engine.liquidation_price(1, btc));
    println!("margin: {:?}", engine.margin(1).unwrap());
}
```

More order builders:

```rust
NewOrder::market(acct, sym, Side::Sell, qty);
NewOrder::limit(acct, sym, Side::Buy, price, qty).post_only();
NewOrder::limit(acct, sym, Side::Buy, price, qty).fok();
NewOrder::limit(acct, sym, Side::Sell, price, qty).iceberg(display_qty);
NewOrder::stop_limit(acct, sym, Side::Sell, trigger, limit, qty).reduce_only();
NewOrder::take_profit_market(acct, sym, Side::Sell, trigger, Qty::ZERO).close_position();
NewOrder::trailing_stop(acct, sym, Side::Sell, TrailingOffset::Rate(Rate::parse("0.01")), None, qty);
```

### Running on its own thread

The engine thread sits behind two lock-free SPSC ring buffers (`src/spsc.rs`):
one for commands and one for events. Each event comes out tagged with the
sequence number of its command, followed by `Output::Done`, so nothing is
allocated per command. The engine thread busy-spins while idle.

```rust
let mut handle = EngineHandle::spawn(Engine::new(), 1 << 12, 1 << 16);
handle.send(Command::AddMarket { spec, price }).unwrap();
handle.send(Command::PlaceOrder(order)).unwrap();

while let Some(out) = handle.recv() {
    match out {
        Output::Event { seq, event } => println!("#{seq}: {event:?}"),
        Output::Done { seq } if seq == 2 => break,
        Output::Done { .. } => {}
    }
}
let engine = handle.shutdown(); // finishes queued commands and returns the final state
```

### Commands

| Command | Purpose |
|---|---|
| `AddMarket` | List a new perpetual contract |
| `Deposit` / `Withdraw` | Move collateral in or out (withdrawals are checked against available margin) |
| `PlaceOrder` / `CancelOrder` / `CancelAll` / `AmendOrder` | Order management |
| `SetLeverage` / `SetMarginMode` / `AdjustIsolatedMargin` | Position settings |
| `SetPositionTpSl` | Position-level TP/SL |
| `MarkPrice` | Mark/index update: fires mark triggers and runs liquidations |
| `Funding` | Apply a funding rate |
| `FundInsurance` / `SetFeeRates` | Admin |

### Events

`OrderAccepted`, `OrderRejected`, `OrderTriggered`, `OrderAmended`, `OrderDone` (with a reason: filled, cancelled, expired, self-trade, OCO, liquidation…), `Trade`, `PositionChanged`, `LeverageChanged`, `MarginModeChanged`, `IsolatedMarginChanged`, `PositionTpSlSet`, `Liquidation`, `AutoDeleverage`, `FundingPayment`, `InsuranceFundChanged`, `BalanceChanged`, `MarkPrice`, `CommandRejected`.

---

## Margin model

For a position of signed size `s`, entry price `E` and mark price `M`:

| Quantity | Formula |
|---|---|
| Notional | `|s| · M` |
| Unrealized PnL | `(M − E) · s` |
| Initial margin | `notional / leverage` |
| Maintenance margin | `notional · mmr(tier) − maint_amount(tier)` |
| Cross equity | `wallet + Σ unrealized PnL (cross)` |
| Available | `equity − Σ initial margin − order margin` |
| Bankruptcy price | `E − margin / s` |

- **Cross** positions are liquidated when `equity < Σ maintenance margin`.
- **Isolated** positions are liquidated when `isolated margin + unrealized PnL < maintenance margin`.

---

## Project layout

```
src/
├── engine.rs       Engine, commands, matching, margin checks, liquidation, ADL, funding
├── book.rs         Limit order book (slab + intrusive lists + BTreeMap levels)
├── conditional.rs  Stop / take-profit / trailing-stop trigger index
├── account.rs      Accounts, positions, open-order margin, margin summary
├── market.rs       Contract spec, risk tiers, per-symbol state
├── order.rs        Order requests (builders) and live orders
├── events.rs       Events, reject and done reasons
├── fixed.rs        Fixed-point Price / Qty / Amount / Rate
├── types.rs        Side, OrderType, TimeInForce, MarginMode, ...
├── runtime.rs      Engine on a dedicated thread
└── bin/bench.rs    Throughput benchmark
tests/engine.rs     Integration tests
examples/           Quickstart
```

---

## Testing

```
cargo test
```

The integration tests in `tests/engine.rs` cover price-time priority, every time-in-force, market slippage protection, self-trade prevention, iceberg refresh, amend priority rules, every conditional order type, attached and position TP/SL with OCO, trailing stops (with and without activation), reduce-only maintenance, margin and risk-limit checks, leverage changes, isolated margin accounting, funding, liquidation through the book, ADL fallback, cross liquidation with insurance-fund coverage, and the threaded runtime.

Several tests also check a global invariant: across all accounts, the long and short positions net to zero.

## Benchmark

```
cargo run --release --bin bench [commands]
```

The benchmark sends a random mix of 60% limit orders, 30% cancels and 10% market orders from 1,000 accounts. A full margin check runs on every order.

On a single core of a laptop it reaches about **1.6M commands/s (~600 ns/command)**, with around 280k resting orders and 800k trades.

---

## Roadmap

- [ ] Hedge mode (simultaneous long and short positions)
- [ ] Inverse (coin-margined) contracts
- [ ] Command journal and snapshots for crash recovery
- [ ] Network gateway (TCP / WebSocket) and market-data feed (L2 deltas)
- [ ] Partial liquidation by risk tier
- [ ] GTD orders (time-aware engine)
- [ ] Mark-price and funding-rate calculation from index and premium

## Acknowledgements

These projects influenced the design:

- [PIYUSH-KUMAR1809/order-matching-engine](https://github.com/PIYUSH-KUMAR1809/order-matching-engine): matching-core architecture and performance techniques
- [MathisWellmann/lfest-rs](https://github.com/MathisWellmann/lfest-rs): type-safe fixed-point newtypes and the leverage/margin model
- [CryptonStudio/crypton-matching-engine](https://github.com/CryptonStudio/crypton-matching-engine): the order-type set (stop, trailing, OCO/TP-SL, iceberg)

## License

[MIT](LICENSE)

## Disclaimer

This is an educational project. It has not been audited and is not intended for production trading with real funds.


