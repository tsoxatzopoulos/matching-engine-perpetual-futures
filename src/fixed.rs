//! Fixed-point numeric newtypes with 8 decimals.
//!
//! Every monetary value in the engine is an `i64` scaled by `10^8`, wrapped in a
//! newtype so that prices, quantities, amounts and rates cannot be mixed up by
//! accident (the lfest-rs approach). Products are computed in `i128` and rounded
//! explicitly.

use std::fmt;
use std::ops::{Add, AddAssign, Neg, Sub, SubAssign};
use std::str::FromStr;

pub const DECIMALS: u32 = 8;
pub const SCALE: i64 = 100_000_000;

/// Rounding direction: `Down` rounds toward -inf, `Up` toward +inf.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Round {
    Down,
    Up,
}

/// `a * b / c` computed in i128 with explicit rounding, saturated to i64.
#[inline]
pub fn mul_div(a: i64, b: i64, c: i64, round: Round) -> i64 {
    debug_assert!(c != 0, "mul_div by zero");
    let n = a as i128 * b as i128;
    let d = c as i128;
    let mut q = n / d;
    let r = n % d;
    if r != 0 {
        let negative = (r < 0) != (d < 0);
        match round {
            Round::Down if negative => q -= 1,
            Round::Up if !negative => q += 1,
            _ => {}
        }
    }
    q.clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParseFixedError;

impl fmt::Display for ParseFixedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid fixed-point number (max {DECIMALS} decimals)")
    }
}

impl std::error::Error for ParseFixedError {}

fn parse_raw(s: &str) -> Result<i64, ParseFixedError> {
    let s = s.trim();
    let (neg, body) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let (int, frac) = body.split_once('.').unwrap_or((body, ""));
    if int.is_empty() && frac.is_empty() {
        return Err(ParseFixedError);
    }
    if !int.bytes().all(|c| c.is_ascii_digit()) || !frac.bytes().all(|c| c.is_ascii_digit()) {
        return Err(ParseFixedError);
    }
    let frac = frac.trim_end_matches('0');
    if frac.len() > DECIMALS as usize {
        return Err(ParseFixedError);
    }
    let int_v: i64 = if int.is_empty() { 0 } else { int.parse().map_err(|_| ParseFixedError)? };
    let frac_v: i64 = if frac.is_empty() {
        0
    } else {
        format!("{frac:0<8}").parse().map_err(|_| ParseFixedError)?
    };
    let raw = int_v
        .checked_mul(SCALE)
        .and_then(|v| v.checked_add(frac_v))
        .ok_or(ParseFixedError)?;
    Ok(if neg { -raw } else { raw })
}

fn fmt_raw(raw: i64, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let sign = if raw < 0 { "-" } else { "" };
    let abs = raw.unsigned_abs();
    let int = abs / SCALE as u64;
    let frac = abs % SCALE as u64;
    if frac == 0 {
        write!(f, "{sign}{int}")
    } else {
        let digits = format!("{frac:08}");
        write!(f, "{sign}{int}.{}", digits.trim_end_matches('0'))
    }
}

macro_rules! fixed_type {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
        pub struct $name(pub i64);

        impl $name {
            pub const ZERO: Self = Self(0);
            pub const ONE: Self = Self(SCALE);

            #[inline]
            pub const fn from_raw(raw: i64) -> Self {
                Self(raw)
            }
            #[inline]
            pub const fn raw(self) -> i64 {
                self.0
            }
            #[inline]
            pub const fn from_int(v: i64) -> Self {
                Self(v * SCALE)
            }
            pub fn from_f64(v: f64) -> Self {
                Self((v * SCALE as f64).round() as i64)
            }
            pub fn to_f64(self) -> f64 {
                self.0 as f64 / SCALE as f64
            }
            /// Parses a decimal literal, panicking on malformed input. Meant for
            /// configuration and tests.
            pub fn parse(s: &str) -> Self {
                s.parse().expect("invalid fixed-point literal")
            }
            #[inline]
            pub fn abs(self) -> Self {
                Self(self.0.abs())
            }
            #[inline]
            pub fn is_zero(self) -> bool {
                self.0 == 0
            }
            #[inline]
            pub fn is_pos(self) -> bool {
                self.0 > 0
            }
            #[inline]
            pub fn is_neg(self) -> bool {
                self.0 < 0
            }
            #[inline]
            pub fn signum(self) -> i64 {
                self.0.signum()
            }
            /// Multiplies by a rate (fee, maintenance ratio, funding rate...).
            #[inline]
            pub fn mul_rate(self, r: Rate, round: Round) -> Self {
                Self(mul_div(self.0, r.0, SCALE, round))
            }
            /// Rounds to a multiple of `step` (tick or lot size).
            #[inline]
            pub fn round_to(self, step: Self, round: Round) -> Self {
                if step.0 <= 0 {
                    return self;
                }
                Self(mul_div(self.0, 1, step.0, round) * step.0)
            }
            #[inline]
            pub fn is_multiple_of(self, step: Self) -> bool {
                step.0 > 0 && self.0 % step.0 == 0
            }
        }

        impl Add for $name {
            type Output = Self;
            #[inline]
            fn add(self, o: Self) -> Self {
                Self(self.0 + o.0)
            }
        }
        impl Sub for $name {
            type Output = Self;
            #[inline]
            fn sub(self, o: Self) -> Self {
                Self(self.0 - o.0)
            }
        }
        impl Neg for $name {
            type Output = Self;
            #[inline]
            fn neg(self) -> Self {
                Self(-self.0)
            }
        }
        impl AddAssign for $name {
            #[inline]
            fn add_assign(&mut self, o: Self) {
                self.0 += o.0;
            }
        }
        impl SubAssign for $name {
            #[inline]
            fn sub_assign(&mut self, o: Self) {
                self.0 -= o.0;
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt_raw(self.0, f)
            }
        }
        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt_raw(self.0, f)
            }
        }
        impl FromStr for $name {
            type Err = ParseFixedError;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                parse_raw(s).map(Self)
            }
        }
    };
}

fixed_type!(
    /// Price in quote currency per contract (e.g. USDT per BTC).
    Price
);
fixed_type!(
    /// Contract quantity. Signed when used as a position size (long > 0).
    Qty
);
fixed_type!(
    /// Quote-currency amount (balances, margin, PnL, fees).
    Amount
);
fixed_type!(
    /// Dimensionless ratio: 0.0005 = 5 bps.
    Rate
);

/// Notional value `price * qty`, rounded down. `qty` may be signed.
#[inline]
pub fn notional(price: Price, qty: Qty) -> Amount {
    Amount(mul_div(price.0, qty.0, SCALE, Round::Down))
}

impl Amount {
    /// Divides by an integer leverage.
    #[inline]
    pub fn div_leverage(self, leverage: u32, round: Round) -> Amount {
        Amount(mul_div(self.0, 1, leverage.max(1) as i64, round))
    }

    /// `amount / qty`, i.e. a price.
    #[inline]
    pub fn div_qty(self, qty: Qty, round: Round) -> Price {
        Price(mul_div(self.0, SCALE, qty.0, round))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_format_roundtrip() {
        for s in ["0", "1", "-1", "0.5", "123.456", "-0.00000001", "99999.12345678"] {
            assert_eq!(Price::parse(s).to_string(), s);
        }
        assert!("1.123456789".parse::<Price>().is_err());
        assert!("abc".parse::<Price>().is_err());
        assert_eq!(Price::parse("1.10").to_string(), "1.1");
    }

    #[test]
    fn mul_div_rounding() {
        assert_eq!(mul_div(7, 1, 2, Round::Down), 3);
        assert_eq!(mul_div(7, 1, 2, Round::Up), 4);
        assert_eq!(mul_div(-7, 1, 2, Round::Down), -4);
        assert_eq!(mul_div(-7, 1, 2, Round::Up), -3);
    }

    #[test]
    fn notional_and_rounding_to_tick() {
        assert_eq!(notional(Price::parse("100.5"), Qty::parse("2")), Amount::parse("201"));
        let tick = Price::parse("0.1");
        assert_eq!(Price::parse("100.07").round_to(tick, Round::Up), Price::parse("100.1"));
        assert_eq!(Price::parse("100.07").round_to(tick, Round::Down), Price::parse("100"));
    }
}
