//! Engine output: every state change is reported as an event.

use crate::fixed::{Amount, Price, Qty, Rate};
use crate::types::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RejectReason {
    UnknownSymbol,
    UnknownAccount,
    UnknownOrder,
    InvalidAmount,
    /// Contract spec failed validation (see `SymbolSpec::validate`).
    InvalidSpec,
    InvalidQty,
    LotSize,
    InvalidPrice,
    MissingPrice,
    MissingTrigger,
    InvalidTif,
    InvalidDisplayQty,
    InvalidTpSl,
    InvalidClosePosition,
    MinNotional,
    PriceOutOfBand,
    WouldImmediatelyTrigger,
    PostOnlyWouldTake,
    FokNotFillable,
    InsufficientMargin,
    InsufficientBalance,
    ReduceOnlyRejected,
    InvalidLeverage,
    RiskLimitExceeded,
    PositionOrOrdersOpen,
    NotIsolated,
    NoPosition,
}

/// Why an order left the system.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DoneReason {
    Filled,
    UserCanceled,
    /// IOC / FOK / market remainder expired.
    Expired,
    SelfTrade,
    ReduceOnly,
    Liquidation,
    /// OCO partner triggered.
    OcoSibling,
    PositionClosed,
    Replaced,
    /// Triggered conditional order rejected on activation.
    Rejected(RejectReason),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BalanceReason {
    Deposit,
    Withdraw,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    MarketAdded {
        symbol: SymbolId,
    },
    BalanceChanged {
        account: AccountId,
        balance: Amount,
        delta: Amount,
        reason: BalanceReason,
    },
    OrderAccepted {
        order_id: OrderId,
        client_order_id: u64,
        account: AccountId,
        symbol: SymbolId,
        side: Side,
        order_type: OrderType,
        price: Price,
        qty: Qty,
        status: OrderStatus,
    },
    OrderRejected {
        client_order_id: u64,
        account: AccountId,
        symbol: SymbolId,
        reason: RejectReason,
    },
    OrderTriggered {
        order_id: OrderId,
        account: AccountId,
        symbol: SymbolId,
        trigger_price: Price,
        market_price: Price,
    },
    OrderAmended {
        order_id: OrderId,
        account: AccountId,
        symbol: SymbolId,
        price: Price,
        qty: Qty,
        trigger_price: Option<Price>,
    },
    OrderDone {
        order_id: OrderId,
        account: AccountId,
        symbol: SymbolId,
        status: OrderStatus,
        filled: Qty,
        avg_price: Price,
        reason: DoneReason,
    },
    Trade {
        trade_id: u64,
        symbol: SymbolId,
        price: Price,
        qty: Qty,
        taker_side: Side,
        taker_order: OrderId,
        maker_order: OrderId,
        taker_account: AccountId,
        maker_account: AccountId,
        taker_fee: Amount,
        maker_fee: Amount,
        liquidation: bool,
    },
    PositionChanged {
        account: AccountId,
        symbol: SymbolId,
        size: Qty,
        entry_price: Price,
        realized_pnl: Amount,
        fee: Amount,
    },
    LeverageChanged {
        account: AccountId,
        symbol: SymbolId,
        leverage: u32,
    },
    MarginModeChanged {
        account: AccountId,
        symbol: SymbolId,
        mode: MarginMode,
    },
    IsolatedMarginChanged {
        account: AccountId,
        symbol: SymbolId,
        isolated_margin: Amount,
        delta: Amount,
    },
    PositionTpSlSet {
        account: AccountId,
        symbol: SymbolId,
        take_profit: Option<OrderId>,
        stop_loss: Option<OrderId>,
    },
    Liquidation {
        account: AccountId,
        symbol: SymbolId,
        side: Side,
        qty: Qty,
        bankruptcy_price: Price,
        mark_price: Price,
    },
    AutoDeleverage {
        symbol: SymbolId,
        liquidated: AccountId,
        counterparty: AccountId,
        counterparty_side: Side,
        qty: Qty,
        price: Price,
    },
    FundingPayment {
        account: AccountId,
        symbol: SymbolId,
        rate: Rate,
        /// Positive = paid by the account.
        amount: Amount,
    },
    InsuranceFundChanged {
        balance: Amount,
        delta: Amount,
    },
    MarkPrice {
        symbol: SymbolId,
        mark: Price,
        index: Price,
    },
    CommandRejected {
        reason: RejectReason,
    },
}
