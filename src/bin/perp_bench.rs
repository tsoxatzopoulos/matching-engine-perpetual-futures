//! Perpetual-futures benchmark: accounts hold positions in every symbol, and
//! mark-price updates and funding run between the orders, as on a live venue.
//!
//! Measured separately: latency per order command (place / cancel), per
//! `MarkPrice` (trigger scan + liquidation check) and per `Funding`.
//!
//! cargo run --release --bin perp_bench [orders_per_config] [accounts:symbols ...]

use std::time::Instant;

use futures_engine::sim::{offset_price, Rng};
use futures_engine::*;

const MARK_EVERY: usize = 500;
const FUNDING_EVERY_MARKS: usize = 20;

struct Config {
    accounts: u64,
    symbols: u32,
}

struct Report {
    positions: usize,
    setup_rejects: usize,
    setup_secs: f64,
    /// First MarkPrice of every symbol after setup (arms every new position).
    warmup_ms: f64,
    orders: Vec<u64>,
    marks: Vec<u64>,
    /// MarkPrice that liquidated nobody: the cost of the risk scan alone.
    marks_quiet: Vec<u64>,
    /// MarkPrice that liquidated someone: scan plus liquidation / ADL.
    marks_liq: Vec<u64>,
    max_liq_per_mark: usize,
    funding: Vec<u64>,
    liquidations: usize,
    adl: usize,
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((p * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len()) - 1;
    sorted[idx]
}

fn mean(v: &[u64]) -> f64 {
    if v.is_empty() { 0.0 } else { v.iter().sum::<u64>() as f64 / v.len() as f64 }
}

fn accepted_id(events: &[Event]) -> Option<OrderId> {
    events.iter().find_map(|e| match e {
        Event::OrderAccepted { order_id, .. } => Some(*order_id),
        _ => None,
    })
}

fn run(cfg: &Config, n_orders: usize) -> Report {
    let mut rng = Rng::new(0xBE7C_4000 + cfg.accounts * 31 + cfg.symbols as u64);
    let mut e = Engine::new();
    let tick = Price::parse("0.01");
    let lot = Qty::parse("0.001");
    let mut marks: Vec<Price> = Vec::new();
    for s in 0..cfg.symbols {
        let price = Price::from_int(1_000 + 100 * s as i64);
        e.process(Command::AddMarket { spec: SymbolSpec::new(&format!("SYM{s}"), tick, lot), price });
        marks.push(price);
    }

    // Accounts: balance, per-symbol margin mode and leverage.
    let setup_start = Instant::now();
    let mut balance = vec![0f64; cfg.accounts as usize + 1];
    let mut leverage = vec![vec![0u32; cfg.symbols as usize]; cfg.accounts as usize + 1];
    for a in 1..=cfg.accounts {
        let b = rng.range(1_000, 50_000);
        balance[a as usize] = b as f64;
        e.process(Command::Deposit { account: a, amount: Amount::from_int(b) });
        for s in 0..cfg.symbols {
            if rng.chance(30) {
                e.process(Command::SetMarginMode { account: a, symbol: s, mode: MarginMode::Isolated });
            }
            let lev = rng.pick(&[5, 10, 20, 50, 100]);
            leverage[a as usize][s as usize] = lev;
            e.process(Command::SetLeverage { account: a, symbol: s, leverage: lev });
        }
    }

    // Positions: pair accounts (2k+1, 2k+2) on opposite sides of every symbol,
    // sized at 20-80% of what their margin allows, so liquidation distance varies.
    let mut setup_rejects = 0;
    for s in 0..cfg.symbols {
        let price = marks[s as usize];
        for a in (1..cfg.accounts).step_by(2) {
            let b = a + 1;
            let cap = |x: u64| balance[x as usize] / cfg.symbols as f64 * leverage[x as usize][s as usize] as f64;
            let notional = cap(a).min(cap(b)) * rng.range(20, 80) as f64 / 100.0;
            let lots = (notional / price.to_f64() / lot.to_f64()) as i64;
            if lots < 10 {
                continue;
            }
            let qty = Qty(lots * lot.0);
            let (sa, sb) = if rng.chance(50) { (Side::Sell, Side::Buy) } else { (Side::Buy, Side::Sell) };
            for (acct, side) in [(a, sa), (b, sb)] {
                let ev = e.process(Command::PlaceOrder(NewOrder::limit(acct, s, side, price, qty)));
                if ev.iter().any(|x| matches!(x, Event::OrderRejected { .. })) {
                    setup_rejects += 1;
                }
            }
        }
        // Drop whatever did not match.
        for a in 1..=cfg.accounts {
            if e.position(a, s).is_some_and(|p| !p.order_ids.is_empty()) {
                e.process(Command::CancelAll { account: a, symbol: Some(s) });
            }
        }
    }
    let setup_secs = setup_start.elapsed().as_secs_f64();
    let positions = e.markets().iter().map(|m| m.holders.len()).sum();

    let warmup = Instant::now();
    for s in 0..cfg.symbols {
        let mark = marks[s as usize];
        e.process(Command::MarkPrice { symbol: s, mark, index: mark });
    }
    let warmup_ms = warmup.elapsed().as_secs_f64() * 1_000.0;

    let mut report = Report {
        positions,
        setup_rejects,
        setup_secs,
        warmup_ms,
        orders: Vec::with_capacity(n_orders),
        marks: Vec::new(),
        marks_quiet: Vec::new(),
        marks_liq: Vec::new(),
        max_liq_per_mark: 0,
        funding: Vec::new(),
        liquidations: 0,
        adl: 0,
    };
    let count = |ev: &[Event], r: &mut Report| {
        for x in ev {
            match x {
                Event::Liquidation { .. } => r.liquidations += 1,
                Event::AutoDeleverage { .. } => r.adl += 1,
                _ => {}
            }
        }
    };

    let mut live: Vec<(AccountId, SymbolId, OrderId)> = Vec::new();
    let mut next_mark_symbol = 0u32;
    for i in 0..n_orders {
        if i % MARK_EVERY == 0 && i > 0 {
            let s = next_mark_symbol;
            next_mark_symbol = (next_mark_symbol + 1) % cfg.symbols;
            let bps = rng.range(-20, 20);
            let mark = offset_price(marks[s as usize], bps, tick);
            marks[s as usize] = mark;
            let t = Instant::now();
            let ev = e.process(Command::MarkPrice { symbol: s, mark, index: mark });
            let ns = t.elapsed().as_nanos() as u64;
            report.marks.push(ns);
            let liqs = ev.iter().filter(|x| matches!(x, Event::Liquidation { .. })).count();
            if liqs == 0 {
                report.marks_quiet.push(ns);
            } else {
                report.marks_liq.push(ns);
                report.max_liq_per_mark = report.max_liq_per_mark.max(liqs);
            }
            count(&ev, &mut report);

            if report.marks.len().is_multiple_of(FUNDING_EVERY_MARKS) {
                let rate = Rate::parse(rng.pick(&["-0.0001", "0.0001", "0.0003"]));
                let t = Instant::now();
                let ev = e.process(Command::Funding { symbol: s, rate });
                report.funding.push(t.elapsed().as_nanos() as u64);
                count(&ev, &mut report);
            }
        }

        let account = 1 + rng.below(cfg.accounts);
        let symbol = rng.below(cfg.symbols as u64) as SymbolId;
        let side = if rng.chance(50) { Side::Buy } else { Side::Sell };
        let qty = Qty(rng.range(1, 20) * lot.0);
        let roll = rng.below(100);
        let cmd = if roll < 55 {
            let bps = match side {
                Side::Buy => rng.range(-50, 5),
                Side::Sell => rng.range(-5, 50),
            };
            let price = offset_price(marks[symbol as usize], bps, tick);
            Command::PlaceOrder(NewOrder::limit(account, symbol, side, price, qty))
        } else if roll < 85 && !live.is_empty() {
            let (account, symbol, order_id) = live.swap_remove(rng.below(live.len() as u64) as usize);
            Command::CancelOrder { account, symbol, order_id }
        } else {
            Command::PlaceOrder(NewOrder::market(account, symbol, side, qty))
        };
        let is_limit = matches!(&cmd, Command::PlaceOrder(o) if o.order_type == OrderType::Limit);

        let t = Instant::now();
        let ev = e.process(cmd);
        report.orders.push(t.elapsed().as_nanos() as u64);

        if is_limit && let Some(id) = accepted_id(&ev) {
            live.push((account, symbol, id));
        }
        count(&ev, &mut report);
    }
    report
}

fn us(ns: u64) -> String {
    format!("{:.2}", ns as f64 / 1_000.0)
}

fn ms(ns: f64) -> String {
    format!("{:.3}", ns / 1_000_000.0)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let n_orders: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(100_000);
    let mut configs: Vec<Config> = args
        .filter_map(|a| {
            let (x, y) = a.split_once(':')?;
            Some(Config { accounts: x.parse().ok()?, symbols: y.parse().ok()? })
        })
        .collect();
    if configs.is_empty() {
        for accounts in [1_000, 10_000, 50_000] {
            for symbols in [1, 10] {
                configs.push(Config { accounts, symbols });
            }
        }
    }

    println!("orders per config: {n_orders}, MarkPrice every {MARK_EVERY} orders, Funding every {FUNDING_EVERY_MARKS} MarkPrice\n");
    println!("### Order commands (place / cancel), µs\n");
    println!("| accounts | symbols | positions | setup s | warm-up ms | p50 | p99 | p99.9 | max | mean |");
    println!("|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|");
    let mut rows = Vec::new();
    for cfg in &configs {
        let mut r = run(cfg, n_orders);
        r.orders.sort_unstable();
        r.marks.sort_unstable();
        r.marks_quiet.sort_unstable();
        r.marks_liq.sort_unstable();
        r.funding.sort_unstable();
        println!(
            "| {} | {} | {} | {:.2} | {:.1} | {} | {} | {} | {} | {:.2} |",
            cfg.accounts,
            cfg.symbols,
            r.positions,
            r.setup_secs,
            r.warmup_ms,
            us(percentile(&r.orders, 0.50)),
            us(percentile(&r.orders, 0.99)),
            us(percentile(&r.orders, 0.999)),
            us(*r.orders.last().unwrap_or(&0)),
            mean(&r.orders) / 1_000.0,
        );
        rows.push((cfg, r));
    }

    println!("\n### MarkPrice and Funding, ms\n");
    println!("| accounts | symbols | MarkPrice n | mean | p50 | p99 | max | Funding n | mean | max | liquidations | ADL | setup rejects |");
    println!("|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|");
    for (cfg, r) in &rows {
        println!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            cfg.accounts,
            cfg.symbols,
            r.marks.len(),
            ms(mean(&r.marks)),
            ms(percentile(&r.marks, 0.50) as f64),
            ms(percentile(&r.marks, 0.99) as f64),
            ms(*r.marks.last().unwrap_or(&0) as f64),
            r.funding.len(),
            ms(mean(&r.funding)),
            ms(*r.funding.last().unwrap_or(&0) as f64),
            r.liquidations,
            r.adl,
            r.setup_rejects,
        );
    }

    println!("\n### MarkPrice split: without vs with liquidations, ms\n");
    println!("| accounts | symbols | quiet n | quiet p50 | quiet max | liquidating n | liq p50 | liq max | max liquidations in one MarkPrice |");
    println!("|---:|---:|---:|---:|---:|---:|---:|---:|---:|");
    for (cfg, r) in &rows {
        println!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            cfg.accounts,
            cfg.symbols,
            r.marks_quiet.len(),
            ms(percentile(&r.marks_quiet, 0.50) as f64),
            ms(*r.marks_quiet.last().unwrap_or(&0) as f64),
            r.marks_liq.len(),
            ms(percentile(&r.marks_liq, 0.50) as f64),
            ms(*r.marks_liq.last().unwrap_or(&0) as f64),
            r.max_liq_per_mark,
        );
    }
}
