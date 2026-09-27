//! Throughput benchmark: a random mix of limit, market and cancel orders
//! around a drifting mid price, with margin checks on every order.
//!
//! cargo run --release --bin bench [orders]

use std::time::Instant;

use futures_engine::*;

/// xorshift64*: deterministic and dependency free.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn main() {
    let n: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(2_000_000);
    const ACCOUNTS: u64 = 1_000;

    let mut e = Engine::new();
    let spec = SymbolSpec::new("BTCUSDT", Price::parse("0.1"), Qty::parse("0.001"));
    let sym = e.add_market(spec, Price::from_int(60_000));
    for acct in 1..=ACCOUNTS {
        e.deposit(acct, Amount::from_int(100_000_000)).unwrap();
    }
    e.take_events();

    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut live: Vec<(AccountId, OrderId)> = Vec::new();
    let (mut trades, mut placed, mut canceled) = (0usize, 0usize, 0usize);

    let start = Instant::now();
    for i in 0..n {
        if i % 10_000 == 0 {
            // keep the mark near the traded price so market orders stay in band
            let last = e.market(sym).unwrap().last_price;
            e.update_mark_price(sym, last, last).unwrap();
        }
        let acct = 1 + rng.below(ACCOUNTS);
        let side = if rng.below(2) == 0 { Side::Buy } else { Side::Sell };
        let qty = Qty::parse("0.001") + Qty(rng.below(100) as i64 * Qty::parse("0.001").raw());
        let roll = rng.below(100);
        let events = if roll < 60 {
            let mid = e.market(sym).unwrap().last_price;
            let offset = Price::parse("0.1").raw() * (rng.below(200) as i64 - 50);
            let price = match side {
                Side::Buy => mid - Price(offset),
                Side::Sell => mid + Price(offset),
            };
            let price = price.round_to(Price::parse("0.1"), Round::Down);
            if let Ok(id) = e.place_order(NewOrder::limit(acct, sym, side, price, qty)) {
                live.push((acct, id));
            }
            placed += 1;
            e.take_events()
        } else if roll < 90 && !live.is_empty() {
            let k = rng.below(live.len() as u64) as usize;
            let (acct, id) = live.swap_remove(k);
            let _ = e.cancel_order(acct, sym, id);
            canceled += 1;
            e.take_events()
        } else {
            let _ = e.place_order(NewOrder::market(acct, sym, side, qty));
            placed += 1;
            e.take_events()
        };
        trades += events.iter().filter(|ev| matches!(ev, Event::Trade { .. })).count();
    }
    let elapsed = start.elapsed();

    let m = e.market(sym).unwrap();
    println!("commands:     {n}");
    println!("placed:       {placed}  canceled: {canceled}  trades: {trades}");
    println!("resting:      {} orders", m.book.len());
    println!("elapsed:      {:.3}s", elapsed.as_secs_f64());
    println!("throughput:   {:.0} commands/s", n as f64 / elapsed.as_secs_f64());
    println!("mean latency: {:.0} ns/command", elapsed.as_nanos() as f64 / n as f64);
}
