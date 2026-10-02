//! Throughput benchmark: a random mix of limit, market and cancel orders
//! around a drifting mid price, with margin checks on every order.
//!
//! cargo run --release --bin bench [commands] [--alloc]
//!
//! `--alloc` uses `Engine::process` (owned `Vec<Event>` per command) instead of
//! the buffer-reusing `Engine::apply`.

use std::time::Instant;

use futures_engine::sim::Rng;
use futures_engine::*;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let alloc = args.iter().any(|a| a == "--alloc");
    let n: usize = args.iter().find_map(|a| a.parse().ok()).unwrap_or(2_000_000);
    const ACCOUNTS: u64 = 1_000;

    let mut e = Engine::new();
    let spec = SymbolSpec::new("BTCUSDT", Price::parse("0.1"), Qty::parse("0.001"));
    let sym = e.add_market(spec, Price::from_int(60_000)).unwrap();
    for acct in 1..=ACCOUNTS {
        e.deposit(acct, Amount::from_int(100_000_000)).unwrap();
    }
    e.take_events();

    let mut rng = Rng::new(0x9E37_79B9_7F4A_7C15);
    let mut live: Vec<(AccountId, OrderId)> = Vec::new();
    let (mut trades, mut placed, mut canceled) = (0usize, 0usize, 0usize);
    let tick = Price::parse("0.1");
    let lot = Qty::parse("0.001");

    let start = Instant::now();
    for i in 0..n {
        if i % 10_000 == 0 {
            // keep the mark near the traded price so market orders stay in band
            let last = e.market(sym).unwrap().last_price;
            e.apply(Command::MarkPrice { symbol: sym, mark: last, index: last });
        }
        let acct = 1 + rng.below(ACCOUNTS);
        let side = if rng.below(2) == 0 { Side::Buy } else { Side::Sell };
        let qty = lot + Qty(rng.below(100) as i64 * lot.raw());
        let roll = rng.below(100);
        let (cmd, is_limit) = if roll < 60 {
            let mid = e.market(sym).unwrap().last_price;
            let offset = tick.raw() * (rng.below(200) as i64 - 50);
            let price = match side {
                Side::Buy => mid - Price(offset),
                Side::Sell => mid + Price(offset),
            };
            let price = price.round_to(tick, Round::Down);
            placed += 1;
            (Command::PlaceOrder(NewOrder::limit(acct, sym, side, price, qty)), true)
        } else if roll < 90 && !live.is_empty() {
            let k = rng.below(live.len() as u64) as usize;
            let (account, order_id) = live.swap_remove(k);
            canceled += 1;
            (Command::CancelOrder { account, symbol: sym, order_id }, false)
        } else {
            placed += 1;
            (Command::PlaceOrder(NewOrder::market(acct, sym, side, qty)), false)
        };

        let owned;
        let events: &[Event] = if alloc {
            owned = e.process(cmd);
            &owned
        } else {
            e.apply(cmd)
        };
        for ev in events {
            match ev {
                Event::Trade { .. } => trades += 1,
                Event::OrderAccepted { order_id, account, .. } if is_limit => live.push((*account, *order_id)),
                _ => {}
            }
        }
    }
    let elapsed = start.elapsed();

    let m = e.market(sym).unwrap();
    println!("mode:         {}", if alloc { "process (Vec per command)" } else { "apply (reused buffer)" });
    println!("commands:     {n}");
    println!("placed:       {placed}  canceled: {canceled}  trades: {trades}");
    println!("resting:      {} orders", m.book.len());
    println!("elapsed:      {:.3}s", elapsed.as_secs_f64());
    println!("throughput:   {:.0} commands/s", n as f64 / elapsed.as_secs_f64());
    println!("mean latency: {:.0} ns/command", elapsed.as_nanos() as f64 / n as f64);
}
