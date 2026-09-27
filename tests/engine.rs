use futures_engine::*;

const S: SymbolId = 0;

fn p(s: &str) -> Price {
    Price::parse(s)
}
fn q(s: &str) -> Qty {
    Qty::parse(s)
}
fn a(s: &str) -> Amount {
    Amount::parse(s)
}
fn r(s: &str) -> Rate {
    Rate::parse(s)
}

fn spec() -> SymbolSpec {
    SymbolSpec::new("TESTUSDT", p("0.1"), q("0.001"))
        .with_fees(r("0.0002"), r("0.0005"))
        .with_price_band(r("0.5"))
}

fn setup_with(spec: SymbolSpec) -> Engine {
    let mut e = Engine::new();
    e.add_market(spec, p("100"));
    for acct in 1..=6 {
        e.deposit(acct, a("100000")).unwrap();
    }
    e.take_events();
    e
}

fn setup() -> Engine {
    setup_with(spec())
}

fn limit(acct: AccountId, side: Side, price: &str, qty: &str) -> NewOrder {
    NewOrder::limit(acct, S, side, p(price), q(qty))
}

fn place(e: &mut Engine, o: NewOrder) -> OrderId {
    e.place_order(o).expect("order accepted")
}

fn size(e: &Engine, acct: AccountId) -> Qty {
    e.position(acct, S).map_or(Qty::ZERO, |p| p.size)
}

fn balance(e: &Engine, acct: AccountId) -> Amount {
    e.account(acct).unwrap().balance
}

/// (maker order, price, qty) of every trade.
fn trades(events: &[Event]) -> Vec<(OrderId, Price, Qty)> {
    events
        .iter()
        .filter_map(|ev| match ev {
            Event::Trade { maker_order, price, qty, .. } => Some((*maker_order, *price, *qty)),
            _ => None,
        })
        .collect()
}

fn done_reason(events: &[Event], id: OrderId) -> Option<DoneReason> {
    events.iter().find_map(|ev| match ev {
        Event::OrderDone { order_id, reason, .. } if *order_id == id => Some(*reason),
        _ => None,
    })
}

/// Every contract has a long and a short side.
fn assert_net_zero(e: &Engine) {
    let mut net = Qty::ZERO;
    for acct in 1..=10 {
        if let Some(pos) = e.position(acct, S) {
            net += pos.size;
        }
    }
    assert_eq!(net, Qty::ZERO, "positions must net to zero");
}

// ---------------------------------------------------------------------------
// Matching and time in force
// ---------------------------------------------------------------------------

#[test]
fn price_time_priority_and_partial_fill() {
    let mut e = setup();
    let o1 = place(&mut e, limit(1, Side::Sell, "100", "1"));
    let o2 = place(&mut e, limit(2, Side::Sell, "100", "1"));
    place(&mut e, limit(3, Side::Sell, "100.5", "1"));
    e.take_events();

    place(&mut e, limit(4, Side::Buy, "100", "1.5"));
    let ev = e.take_events();
    assert_eq!(trades(&ev), vec![(o1, p("100"), q("1")), (o2, p("100"), q("0.5"))]);

    let depth = e.depth(S, 5).unwrap();
    assert_eq!(depth.asks, vec![(p("100"), q("0.5")), (p("100.5"), q("1"))]);
    assert!(depth.bids.is_empty());
    assert_eq!(size(&e, 4), q("1.5"));
    assert_eq!(size(&e, 1), q("-1"));
    assert_eq!(size(&e, 2), q("-0.5"));
    assert_net_zero(&e);
}

#[test]
fn taker_gets_maker_price_and_fees() {
    let mut e = setup();
    place(&mut e, limit(1, Side::Sell, "99", "1"));
    place(&mut e, limit(2, Side::Buy, "100", "1"));
    assert_eq!(e.position(2, S).unwrap().entry_price, p("99"));
    // taker 0.05% of 99, maker 0.02% of 99
    assert_eq!(balance(&e, 2), a("100000") - a("0.0495"));
    assert_eq!(balance(&e, 1), a("100000") - a("0.0198"));
}

#[test]
fn ioc_expires_remainder() {
    let mut e = setup();
    place(&mut e, limit(1, Side::Sell, "100", "1"));
    let id = place(&mut e, limit(2, Side::Buy, "100", "2").ioc());
    let ev = e.take_events();
    assert_eq!(trades(&ev).len(), 1);
    assert_eq!(done_reason(&ev, id), Some(DoneReason::Expired));
    assert!(e.depth(S, 5).unwrap().bids.is_empty());
}

#[test]
fn fok_all_or_nothing() {
    let mut e = setup();
    place(&mut e, limit(1, Side::Sell, "100", "1"));
    assert_eq!(e.place_order(limit(2, Side::Buy, "100", "2").fok()), Err(RejectReason::FokNotFillable));
    assert!(trades(&e.take_events()).is_empty());
    place(&mut e, limit(2, Side::Buy, "100", "1").fok());
    assert_eq!(size(&e, 2), q("1"));
}

#[test]
fn post_only_never_takes() {
    let mut e = setup();
    place(&mut e, limit(1, Side::Sell, "100", "1"));
    assert_eq!(e.place_order(limit(2, Side::Buy, "100", "1").post_only()), Err(RejectReason::PostOnlyWouldTake));
    place(&mut e, limit(2, Side::Buy, "99.9", "1").post_only());
    assert_eq!(e.depth(S, 1).unwrap().bids, vec![(p("99.9"), q("1"))]);
}

#[test]
fn market_order_slippage_protection() {
    let mut e = setup_with(spec().with_market_slippage(r("0.05")));
    place(&mut e, limit(1, Side::Sell, "100", "1"));
    place(&mut e, limit(1, Side::Sell, "104", "1"));
    place(&mut e, limit(1, Side::Sell, "106", "1"));
    e.take_events();
    let id = place(&mut e, NewOrder::market(2, S, Side::Buy, q("3")));
    let ev = e.take_events();
    let prices: Vec<Price> = trades(&ev).iter().map(|t| t.1).collect();
    assert_eq!(prices, vec![p("100"), p("104")]); // mark 100 * 1.05 = 105 cap
    assert_eq!(done_reason(&ev, id), Some(DoneReason::Expired));
    assert_eq!(size(&e, 2), q("2"));
}

#[test]
fn self_trade_prevention_modes() {
    let mut e = setup();
    let maker = place(&mut e, limit(1, Side::Sell, "100", "1"));
    e.take_events();
    // default: cancel maker, taker continues and rests
    place(&mut e, limit(1, Side::Buy, "100", "1"));
    let ev = e.take_events();
    assert_eq!(done_reason(&ev, maker), Some(DoneReason::SelfTrade));
    assert!(trades(&ev).is_empty());
    assert_eq!(e.depth(S, 1).unwrap().bids, vec![(p("100"), q("1"))]);

    // cancel taker: resting bid stays
    let taker = place(&mut e, limit(1, Side::Sell, "100", "1").stp(StpMode::CancelTaker));
    let ev = e.take_events();
    assert_eq!(done_reason(&ev, taker), Some(DoneReason::SelfTrade));
    assert_eq!(e.depth(S, 1).unwrap().bids, vec![(p("100"), q("1"))]);
}

#[test]
fn iceberg_shows_slice_and_requeues() {
    let mut e = setup();
    let ice = place(&mut e, limit(1, Side::Sell, "100", "3").iceberg(q("1")));
    let other = place(&mut e, limit(2, Side::Sell, "100", "1"));
    assert_eq!(e.depth(S, 1).unwrap().asks, vec![(p("100"), q("2"))]);
    e.take_events();

    place(&mut e, limit(3, Side::Buy, "100", "2"));
    let makers: Vec<OrderId> = trades(&e.take_events()).iter().map(|t| t.0).collect();
    // refreshed slice goes behind the other order
    assert_eq!(makers, vec![ice, other]);
    assert_eq!(e.depth(S, 1).unwrap().asks, vec![(p("100"), q("1"))]);
    assert_eq!(e.order(S, ice).unwrap().leaves(), q("2"));
}

#[test]
fn amend_down_keeps_priority_amend_up_loses_it() {
    let mut e = setup();
    let o1 = place(&mut e, limit(1, Side::Sell, "100", "1"));
    let o2 = place(&mut e, limit(2, Side::Sell, "100", "1"));
    e.amend_order(1, S, o1, None, Some(q("0.5")), None).unwrap();
    e.take_events();
    place(&mut e, limit(3, Side::Buy, "100", "0.2"));
    assert_eq!(trades(&e.take_events())[0].0, o1);

    e.amend_order(1, S, o1, None, Some(q("2")), None).unwrap();
    e.take_events();
    place(&mut e, limit(3, Side::Buy, "100", "0.2"));
    assert_eq!(trades(&e.take_events())[0].0, o2);
}

#[test]
fn amend_price_that_crosses_executes() {
    let mut e = setup();
    place(&mut e, limit(1, Side::Sell, "101", "1"));
    let bid = place(&mut e, limit(2, Side::Buy, "99", "1"));
    e.take_events();
    e.amend_order(2, S, bid, Some(p("101")), None, None).unwrap();
    assert_eq!(trades(&e.take_events()).len(), 1);
    assert_eq!(size(&e, 2), q("1"));
}

#[test]
fn cancel_and_cancel_all() {
    let mut e = setup();
    let o1 = place(&mut e, limit(1, Side::Buy, "99", "1"));
    place(&mut e, limit(1, Side::Buy, "98", "1"));
    place(&mut e, NewOrder::stop_market(1, S, Side::Buy, p("110"), q("1")));
    assert_eq!(e.cancel_order(2, S, o1), Err(RejectReason::UnknownOrder));
    e.cancel_order(1, S, o1).unwrap();
    assert_eq!(e.cancel_all(1, None), Ok(2));
    assert!(e.open_orders(1, S).is_empty());
    assert!(e.depth(S, 5).unwrap().bids.is_empty());
    assert_eq!(e.position(1, S).unwrap().open, OpenOrders::default());
}

#[test]
fn validation_rejects() {
    let mut e = setup();
    assert_eq!(e.place_order(limit(1, Side::Buy, "100.05", "1")), Err(RejectReason::InvalidPrice));
    assert_eq!(e.place_order(limit(1, Side::Buy, "100", "0.0005")), Err(RejectReason::InvalidQty));
    assert_eq!(e.place_order(limit(1, Side::Buy, "100", "0.0015")), Err(RejectReason::LotSize));
    assert_eq!(e.place_order(limit(1, Side::Buy, "100", "0.01")), Err(RejectReason::MinNotional));
    assert_eq!(e.place_order(limit(99, Side::Buy, "100", "1")), Err(RejectReason::UnknownAccount));
    assert_eq!(e.place_order(limit(1, Side::Buy, "151", "1")), Err(RejectReason::PriceOutOfBand));
    // stop buy below the market would fire immediately
    assert_eq!(
        e.place_order(NewOrder::stop_market(1, S, Side::Buy, p("99"), q("1"))),
        Err(RejectReason::WouldImmediatelyTrigger)
    );
    // TP of a long must be above the entry price
    assert_eq!(
        e.place_order(limit(1, Side::Buy, "100", "1").take_profit(TpSl::market(p("95")))),
        Err(RejectReason::InvalidTpSl)
    );
}

// ---------------------------------------------------------------------------
// Conditional orders
// ---------------------------------------------------------------------------

#[test]
fn stop_market_triggers_on_last_price() {
    let mut e = setup();
    e.update_mark_price(S, p("105"), p("105")).unwrap();
    place(&mut e, limit(1, Side::Sell, "105", "1"));
    place(&mut e, limit(2, Side::Sell, "106", "1"));
    let stop = place(&mut e, NewOrder::stop_market(3, S, Side::Buy, p("105"), q("1")));
    assert_eq!(e.order(S, stop).unwrap().status, OrderStatus::Untriggered);
    e.take_events();

    place(&mut e, limit(4, Side::Buy, "105", "1")); // trades at 105 -> stop fires
    let ev = e.take_events();
    assert!(ev.iter().any(|x| matches!(x, Event::OrderTriggered { order_id, .. } if *order_id == stop)));
    assert_eq!(trades(&ev).last().unwrap().1, p("106"));
    assert_eq!(size(&e, 3), q("1"));
    assert_net_zero(&e);
}

#[test]
fn stop_limit_rests_after_trigger() {
    let mut e = setup();
    place(&mut e, limit(1, Side::Buy, "95", "1"));
    let stop = place(&mut e, NewOrder::stop_limit(3, S, Side::Sell, p("95"), p("94"), q("2")));
    place(&mut e, limit(2, Side::Sell, "95", "1")); // last = 95
    // stop-limit sells 2 @ 94: nothing left to hit, so it rests
    let o = e.order(S, stop).unwrap();
    assert_eq!(o.status, OrderStatus::New);
    assert_eq!(e.depth(S, 1).unwrap().asks, vec![(p("94"), q("2"))]);
}

#[test]
fn mark_price_trigger() {
    let mut e = setup();
    place(&mut e, limit(1, Side::Buy, "97", "1"));
    place(&mut e, NewOrder::stop_market(2, S, Side::Sell, p("98"), q("1")).trigger_by(TriggerBy::MarkPrice));
    e.update_mark_price(S, p("98"), p("98")).unwrap();
    assert_eq!(size(&e, 2), q("-1"));
}

#[test]
fn attached_tp_sl_are_oco() {
    let mut e = setup();
    place(&mut e, limit(1, Side::Sell, "100", "1"));
    place(
        &mut e,
        limit(2, Side::Buy, "100", "1").take_profit(TpSl::market(p("110"))).stop_loss(TpSl::market(p("90"))),
    );
    let children = e.open_orders(2, S);
    assert_eq!(children.len(), 2);
    assert!(children.iter().all(|o| o.reduce_only && o.qty == q("1") && o.side == Side::Sell));
    let sl = children.iter().find(|o| o.order_type == OrderType::StopMarket).unwrap().id;
    e.take_events();

    place(&mut e, limit(3, Side::Buy, "110", "2"));
    place(&mut e, limit(4, Side::Sell, "110", "1")); // last = 110 -> TP fires
    let ev = e.take_events();
    assert_eq!(done_reason(&ev, sl), Some(DoneReason::OcoSibling));
    assert_eq!(size(&e, 2), Qty::ZERO);
    assert_eq!(e.position(2, S).unwrap().realized_pnl, a("10"));
    assert!(e.open_orders(2, S).is_empty());
    assert_net_zero(&e);
}

#[test]
fn attached_tp_sl_grow_with_partial_fills() {
    let mut e = setup();
    let parent = place(&mut e, limit(2, Side::Buy, "100", "3").stop_loss(TpSl::market(p("90"))));
    place(&mut e, limit(1, Side::Sell, "100", "1"));
    place(&mut e, limit(1, Side::Sell, "100", "1"));
    let sl = e.order(S, parent).unwrap().children[1].unwrap();
    assert_eq!(e.order(S, sl).unwrap().qty, q("2"));
}

#[test]
fn trailing_stop_follows_price() {
    let mut e = setup();
    place(&mut e, limit(1, Side::Sell, "100", "1"));
    place(&mut e, limit(2, Side::Buy, "100", "1"));
    let ts = place(
        &mut e,
        NewOrder::trailing_stop(2, S, Side::Sell, TrailingOffset::Absolute(p("5")), None, q("1")).reduce_only(),
    );
    assert_eq!(e.order(S, ts).unwrap().trigger.unwrap().price, p("95"));

    place(&mut e, limit(3, Side::Buy, "110", "1"));
    place(&mut e, limit(4, Side::Sell, "110", "1")); // high 110 -> stop 105
    assert_eq!(e.order(S, ts).unwrap().trigger.unwrap().price, p("105"));

    place(&mut e, limit(5, Side::Buy, "104", "2"));
    place(&mut e, limit(6, Side::Sell, "104", "1")); // 104 <= 105 fires
    assert!(e.order(S, ts).is_none());
    assert_eq!(size(&e, 2), Qty::ZERO);
    assert_net_zero(&e);
}

#[test]
fn trailing_stop_with_activation_price() {
    let mut e = setup();
    let ts = place(
        &mut e,
        NewOrder::trailing_stop(1, S, Side::Buy, TrailingOffset::Rate(r("0.01")), Some(p("95")), q("1")),
    );
    assert!(!e.order(S, ts).unwrap().trigger.unwrap().trailing.unwrap().activated);
    place(&mut e, limit(2, Side::Buy, "95", "1"));
    place(&mut e, limit(3, Side::Sell, "95", "1"));
    let t = e.order(S, ts).unwrap().trigger.unwrap();
    assert!(t.trailing.unwrap().activated);
    assert_eq!(t.price, p("95.95"));
}

#[test]
fn position_tp_sl_closes_whole_position() {
    let mut e = setup();
    place(&mut e, limit(1, Side::Sell, "100", "2"));
    place(&mut e, limit(2, Side::Buy, "100", "2"));
    e.set_position_tpsl(2, S, Some(TpSl::market(p("110"))), Some(TpSl::market(p("90")))).unwrap();
    assert_eq!(e.open_orders(2, S).len(), 2);

    e.update_mark_price(S, p("90"), p("90")).unwrap();
    place(&mut e, limit(3, Side::Buy, "90", "3"));
    place(&mut e, limit(4, Side::Sell, "90", "1")); // last = 90 -> SL closes 2
    assert_eq!(size(&e, 2), Qty::ZERO);
    assert!(e.open_orders(2, S).is_empty());
    assert_net_zero(&e);
}

// ---------------------------------------------------------------------------
// Reduce-only
// ---------------------------------------------------------------------------

#[test]
fn reduce_only_clamps_and_rejects() {
    let mut e = setup();
    place(&mut e, limit(1, Side::Sell, "100", "1"));
    place(&mut e, limit(2, Side::Buy, "100", "1"));
    assert_eq!(e.place_order(limit(2, Side::Buy, "99", "1").reduce_only()), Err(RejectReason::ReduceOnlyRejected));
    let ro = place(&mut e, limit(2, Side::Sell, "105", "5").reduce_only());
    assert_eq!(e.order(S, ro).unwrap().qty, q("1"));

    // closing the position elsewhere cancels the reduce-only order
    place(&mut e, limit(3, Side::Buy, "99", "1"));
    e.take_events();
    place(&mut e, NewOrder::market(2, S, Side::Sell, q("1")));
    let ev = e.take_events();
    assert_eq!(size(&e, 2), Qty::ZERO);
    assert_eq!(done_reason(&ev, ro), Some(DoneReason::PositionClosed));
}

// ---------------------------------------------------------------------------
// Margin and leverage
// ---------------------------------------------------------------------------

#[test]
fn margin_check_and_leverage() {
    let mut e = setup();
    e.deposit(7, a("100")).unwrap();
    // 3000 notional at 20x needs 150
    assert_eq!(e.place_order(limit(7, Side::Buy, "100", "30")), Err(RejectReason::InsufficientMargin));
    e.set_leverage(7, S, 50).unwrap();
    place(&mut e, limit(7, Side::Buy, "100", "30"));
    assert_eq!(e.margin(7).unwrap().order_margin, a("60"));
    // lowering leverage would need 300
    assert_eq!(e.set_leverage(7, S, 10), Err(RejectReason::InsufficientMargin));
    assert_eq!(e.position(7, S).unwrap().leverage, 50);
    assert_eq!(e.set_leverage(7, S, 200), Err(RejectReason::InvalidLeverage));
}

#[test]
fn closing_orders_need_no_margin() {
    let mut e = setup();
    e.deposit(7, a("100")).unwrap();
    e.set_leverage(7, S, 10).unwrap();
    place(&mut e, limit(1, Side::Sell, "100", "9"));
    place(&mut e, limit(7, Side::Buy, "100", "9"));
    // sell 9 only reduces: no extra margin
    place(&mut e, limit(7, Side::Sell, "120", "9"));
    // sell 18 would open a 9 short: needs 90 more, not available
    assert_eq!(e.place_order(limit(7, Side::Sell, "120", "9")), Err(RejectReason::InsufficientMargin));
}

#[test]
fn risk_limit_caps_leverage() {
    let mut e = Engine::new();
    let spec = spec().with_risk_tiers(&[(a("1000"), 50, r("0.01")), (a("10000"), 10, r("0.05"))]);
    e.add_market(spec, p("100"));
    e.deposit(1, a("100000")).unwrap();
    e.set_leverage(1, S, 50).unwrap();
    // 2000 notional falls in tier 2: max 10x
    assert_eq!(e.place_order(limit(1, Side::Buy, "100", "20")), Err(RejectReason::RiskLimitExceeded));
    e.set_leverage(1, S, 10).unwrap();
    place(&mut e, limit(1, Side::Buy, "100", "20"));
    assert_eq!(e.place_order(limit(1, Side::Buy, "100", "90")), Err(RejectReason::RiskLimitExceeded));
}

#[test]
fn margin_mode_switch_requires_flat() {
    let mut e = setup();
    place(&mut e, limit(1, Side::Sell, "100", "1"));
    place(&mut e, limit(2, Side::Buy, "100", "1"));
    assert_eq!(e.set_margin_mode(2, S, MarginMode::Isolated), Err(RejectReason::PositionOrOrdersOpen));
    e.set_margin_mode(3, S, MarginMode::Isolated).unwrap();
}

#[test]
fn isolated_margin_accounting_and_adjust() {
    let mut e = setup();
    e.deposit(7, a("1000")).unwrap();
    e.set_margin_mode(7, S, MarginMode::Isolated).unwrap();
    e.set_leverage(7, S, 10).unwrap();
    place(&mut e, limit(1, Side::Sell, "100", "10"));
    place(&mut e, limit(7, Side::Buy, "100", "10"));
    let pos = e.position(7, S).unwrap();
    assert_eq!(pos.isolated_margin, a("100"));
    assert_eq!(balance(&e, 7), a("899.5")); // 1000 - 100 margin - 0.5 fee

    let liq = e.liquidation_price(7, S).unwrap();
    assert!(liq > p("90.36") && liq < p("90.37"), "liq {liq}");

    e.adjust_isolated_margin(7, S, a("50")).unwrap();
    assert!(e.liquidation_price(7, S).unwrap() < liq);
    assert_eq!(e.adjust_isolated_margin(7, S, a("-100")), Err(RejectReason::InsufficientMargin));
    e.adjust_isolated_margin(7, S, a("-50")).unwrap();

    // closing returns collateral plus PnL
    place(&mut e, limit(2, Side::Buy, "110", "10"));
    place(&mut e, NewOrder::market(7, S, Side::Sell, q("10")));
    assert_eq!(e.position(7, S).unwrap().isolated_margin, Amount::ZERO);
    assert_eq!(balance(&e, 7), a("899.5") + a("100") + a("100") - a("0.55"));
}

#[test]
fn withdraw_respects_available() {
    let mut e = setup();
    e.deposit(7, a("100")).unwrap();
    place(&mut e, limit(7, Side::Buy, "100", "19")); // 95 margin at 20x
    assert_eq!(e.withdraw(7, a("10")), Err(RejectReason::InsufficientBalance));
    e.withdraw(7, a("4")).unwrap();
}

// ---------------------------------------------------------------------------
// Funding, liquidation, ADL
// ---------------------------------------------------------------------------

#[test]
fn funding_moves_from_longs_to_shorts() {
    let mut e = setup_with(spec().with_fees(Rate::ZERO, Rate::ZERO));
    place(&mut e, limit(1, Side::Sell, "100", "1"));
    place(&mut e, limit(2, Side::Buy, "100", "1"));
    e.apply_funding(S, r("0.0001")).unwrap();
    assert_eq!(balance(&e, 2), a("100000") - a("0.01"));
    assert_eq!(balance(&e, 1), a("100000") + a("0.01"));
}

/// Account 7: 10 long @100, 10x isolated, 100 collateral.
fn isolated_long(e: &mut Engine) {
    e.deposit(7, a("1000")).unwrap();
    e.set_margin_mode(7, S, MarginMode::Isolated).unwrap();
    e.set_leverage(7, S, 10).unwrap();
    place(e, limit(1, Side::Sell, "100", "10"));
    place(e, limit(7, Side::Buy, "100", "10"));
}

#[test]
fn isolated_liquidation_through_book() {
    let mut e = setup();
    isolated_long(&mut e);
    place(&mut e, limit(3, Side::Buy, "90.5", "10"));

    e.update_mark_price(S, p("90.5"), p("90.5")).unwrap(); // equity 5 > mm 3.62
    assert_eq!(size(&e, 7), q("10"));
    e.take_events();

    e.update_mark_price(S, p("90.3"), p("90.3")).unwrap(); // equity 3 < mm 3.612
    let ev = e.take_events();
    assert!(ev.iter().any(|x| matches!(x, Event::Liquidation { account: 7, bankruptcy_price, .. } if *bankruptcy_price == p("90"))));
    assert_eq!(size(&e, 7), Qty::ZERO);
    assert_eq!(size(&e, 3), q("10"));
    // filled at 90.5: 5 left minus 0.4525 taker fee, all taken as clearance fee
    assert_eq!(e.insurance_fund(), a("4.5475"));
    assert_eq!(balance(&e, 7), a("899.5"));
    assert_net_zero(&e);
}

#[test]
fn liquidation_falls_back_to_adl() {
    let mut e = setup();
    isolated_long(&mut e);
    e.update_mark_price(S, p("90.3"), p("90.3")).unwrap();
    let ev = e.take_events();
    assert!(ev.iter().any(|x| matches!(
        x,
        Event::AutoDeleverage { liquidated: 7, counterparty: 1, qty, price, .. } if *qty == q("10") && *price == p("90")
    )));
    assert_eq!(size(&e, 7), Qty::ZERO);
    assert_eq!(size(&e, 1), Qty::ZERO);
    assert_eq!(e.position(1, S).unwrap().realized_pnl, a("100"));
    assert_eq!(balance(&e, 7), a("899.5"));
    assert_net_zero(&e);
}

#[test]
fn cross_liquidation_and_insurance() {
    let mut e = setup();
    e.deposit(7, a("100")).unwrap();
    place(&mut e, limit(1, Side::Sell, "100", "10"));
    place(&mut e, limit(7, Side::Buy, "100", "10")); // 20x, 50 IM, fee 0.5
    e.update_mark_price(S, p("90.5"), p("90.5")).unwrap();
    assert_eq!(size(&e, 7), q("10"));

    e.update_mark_price(S, p("90.3"), p("90.3")).unwrap();
    assert_eq!(size(&e, 7), Qty::ZERO);
    // bankruptcy 100 - 99.5/10 = 90.05 -> 90.1; realized -99 leaves 0.5 -> fee
    assert_eq!(balance(&e, 7), Amount::ZERO);
    assert_eq!(e.insurance_fund(), a("0.5"));
    assert_net_zero(&e);
}

#[test]
fn liquidation_cancels_orders_first() {
    let mut e = setup();
    isolated_long(&mut e);
    let tp = place(&mut e, limit(7, Side::Sell, "120", "10").reduce_only());
    e.take_events();
    e.update_mark_price(S, p("90.3"), p("90.3")).unwrap();
    assert_eq!(done_reason(&e.take_events(), tp), Some(DoneReason::Liquidation));
}

// ---------------------------------------------------------------------------
// Runtime
// ---------------------------------------------------------------------------

#[test]
fn engine_thread_round_trip() {
    let h = EngineHandle::spawn(Engine::new(), 1024);
    h.send(Command::AddMarket { spec: spec(), price: p("100") }).unwrap();
    h.send(Command::Deposit { account: 1, amount: a("1000") }).unwrap();
    h.send(Command::Deposit { account: 2, amount: a("1000") }).unwrap();
    h.send(Command::PlaceOrder(limit(1, Side::Sell, "100", "1"))).unwrap();
    h.send(Command::PlaceOrder(limit(2, Side::Buy, "100", "1"))).unwrap();
    let batches: Vec<(u64, Vec<Event>)> = (0..5).map(|_| h.events().recv().unwrap()).collect();
    assert_eq!(batches.iter().map(|b| b.0).collect::<Vec<_>>(), vec![1, 2, 3, 4, 5]);
    assert_eq!(trades(&batches[4].1).len(), 1);
    let engine = h.shutdown();
    assert_eq!(engine.position(2, S).unwrap().size, q("1"));
}
