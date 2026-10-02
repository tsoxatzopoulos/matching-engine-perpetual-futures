use futures_engine::*;

fn main() {
    let mut e = Engine::new();
    let btc = e.add_market(SymbolSpec::new("BTCUSDT", Price::parse("0.1"), Qty::parse("0.001")),
                           Price::parse("60000")).unwrap();
    e.deposit(1, Amount::parse("10000")).unwrap();
    e.deposit(2, Amount::parse("10000")).unwrap();
    e.set_leverage(1, btc, 20).unwrap();

    e.place_order(NewOrder::limit(2, btc, Side::Sell, Price::parse("60000"), Qty::parse("0.1"))).unwrap();
    e.place_order(
        NewOrder::limit(1, btc, Side::Buy, Price::parse("60000"), Qty::parse("0.1"))
            .take_profit(TpSl::market(Price::parse("63000")))
            .stop_loss(TpSl::market(Price::parse("58000")).by(TriggerBy::MarkPrice)),
    ).unwrap();

    for ev in e.take_events() { println!("{ev:?}"); }
    println!("liq price: {:?}", e.liquidation_price(1, btc));
    println!("margin:    {:?}", e.margin(1).unwrap());
}
