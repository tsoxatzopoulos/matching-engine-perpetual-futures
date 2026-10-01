//! Crypto perpetual-futures matching engine.
//!
//! * Central limit order book with price-time priority (`book`)
//! * Order types: limit, market, stop, stop-limit, take-profit (market/limit),
//!   trailing stop; TIF GTC / IOC / FOK / post-only; reduce-only,
//!   close-position, iceberg, self-trade prevention, amend
//! * Attached TP/SL on entry orders and position TP/SL, OCO-linked
//! * Linear (USDT-margined) contracts with cross and isolated margin,
//!   per-symbol leverage, tiered maintenance margin (risk limits)
//! * Mark-price liquidation, insurance fund, auto-deleveraging, funding

pub mod account;
pub mod book;
pub mod conditional;
pub mod engine;
pub mod events;
pub mod fixed;
pub mod hash;
pub mod market;
pub mod order;
pub mod risk;
pub mod runtime;
pub mod sim;
pub mod spsc;
pub mod types;

pub use account::{Account, MarginSummary, OpenOrders, Position};
pub use engine::{Command, Depth, Engine};
pub use events::{BalanceReason, DoneReason, Event, RejectReason};
pub use fixed::{Amount, Price, Qty, Rate, Round};
pub use market::{Market, RiskTier, SymbolSpec};
pub use order::{NewOrder, Order, TpSl, TrailingOffset, TrailingSpec};
pub use runtime::{EngineHandle, EngineStopped, Output};
pub use types::*;
