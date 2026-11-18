//! Currency conversion with ECB euro foreign exchange reference rates.
//!
//! The ECB publishes, per business day, how many units of ~30 currencies one euro
//! buys. Conversion between two currencies goes through EUR (cross rates) using
//! the latest rate on or before the requested date. Currencies the ECB does not
//! cover (AED, SAR, TWD, …) cannot be converted.

use std::collections::{BTreeMap, HashMap};
use std::fmt;

use chrono::NaiveDate;

use crate::Money;
use crate::billing::div_round;
use crate::currency::minor_unit_exponent;

/// Units of a currency per 1 EUR, as the exact decimal the ECB published
/// (`mantissa / 10^scale`). Always positive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PerEur {
    mantissa: u64,
    scale: u32,
}

impl PerEur {
    /// Parses a plain positive decimal (`"1.1355"`, `"137"`), at most 18 digits.
    pub fn parse(s: &str) -> Option<PerEur> {
        let (int, frac) = s.split_once('.').unwrap_or((s, ""));
        let digits = format!("{int}{frac}");
        if int.is_empty() || digits.len() > 18 || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let mantissa: u64 = digits.parse().ok()?;
        (mantissa > 0).then_some(PerEur {
            mantissa,
            scale: frac.len() as u32,
        })
    }
}

impl fmt::Display for PerEur {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let scale = self.scale as usize;
        let digits = format!("{:0>width$}", self.mantissa, width = scale + 1);
        let (int, frac) = digits.split_at(digits.len() - scale);
        if frac.is_empty() {
            f.write_str(int)
        } else {
            write!(f, "{int}.{frac}")
        }
    }
}

/// One day of ECB reference rates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DailyRates {
    pub date: NaiveDate,
    /// `(ISO 4217 code, rate)`; EUR itself is implicit (always 1).
    pub rates: Vec<(String, PerEur)>,
}

#[derive(Debug, thiserror::Error)]
#[error("invalid ECB rates XML: {0}")]
pub struct EcbParseError(String);

/// Parses `eurofxref-daily.xml` / `eurofxref-hist*.xml`:
/// `<Cube time='YYYY-MM-DD'><Cube currency='USD' rate='1.1355'/>…</Cube>…`.
///
/// Hand-rolled instead of a full XML parser: the format is flat, fixed and
/// attribute-only, so scanning `<Cube …>` tags is enough. Anything unexpected in
/// those tags (bad date, code or rate, rates outside a dated cube, no data at
/// all, e.g. an HTML error page) is an error rather than silently skipped.
pub fn parse_ecb_xml(xml: &str) -> Result<Vec<DailyRates>, EcbParseError> {
    let err = |msg: String| EcbParseError(msg);
    let mut days: Vec<DailyRates> = Vec::new();
    for tag in xml.split("<Cube").skip(1) {
        let tag = &tag[..tag
            .find('>')
            .ok_or_else(|| err("unterminated Cube tag".into()))?];
        if let Some(time) = attr(tag, "time") {
            let date = NaiveDate::parse_from_str(time, "%Y-%m-%d")
                .map_err(|_| err(format!("bad date `{time}`")))?;
            days.push(DailyRates {
                date,
                rates: Vec::new(),
            });
        } else if let Some(currency) = attr(tag, "currency") {
            if currency.len() != 3 || !currency.bytes().all(|b| b.is_ascii_uppercase()) {
                return Err(err(format!("bad currency `{currency}`")));
            }
            let rate = attr(tag, "rate")
                .and_then(PerEur::parse)
                .ok_or_else(|| err(format!("bad rate for {currency}")))?;
            days.last_mut()
                .ok_or_else(|| err(format!("{currency} rate outside a dated Cube")))?
                .rates
                .push((currency.to_owned(), rate));
        }
    }
    if days.is_empty() || days.iter().any(|d| d.rates.is_empty()) {
        return Err(err("no rates".into()));
    }
    Ok(days)
}

/// Value of attribute `name` (single- or double-quoted) in the inside of a tag.
fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let mut rest = tag;
    while let Some(i) = rest.find(name) {
        let preceded_by_space = rest[..i].ends_with(|c: char| c.is_ascii_whitespace());
        let after = &rest[i + name.len()..];
        if preceded_by_space && let Some(value) = after.strip_prefix('=') {
            let quote = value.chars().next().filter(|q| matches!(q, '\'' | '"'))?;
            let value = &value[1..];
            return value.find(quote).map(|end| &value[..end]);
        }
        rest = after;
    }
    None
}

/// A history of reference rates, for converting between currencies on a date.
#[derive(Debug, Clone, Default)]
pub struct FxRates {
    by_currency: HashMap<String, BTreeMap<NaiveDate, PerEur>>,
}

impl FxRates {
    pub fn insert(&mut self, date: NaiveDate, currency: &str, rate: PerEur) {
        self.by_currency
            .entry(currency.to_ascii_uppercase())
            .or_default()
            .insert(date, rate);
    }

    /// Latest rate on or before `date`. EUR is always 1.
    fn rate_on(&self, currency: &str, date: NaiveDate) -> Option<PerEur> {
        let currency = currency.to_ascii_uppercase();
        if currency == "EUR" {
            return Some(PerEur {
                mantissa: 1,
                scale: 0,
            });
        }
        self.by_currency
            .get(&currency)?
            .range(..=date)
            .next_back()
            .map(|(_, rate)| *rate)
    }

    /// Converts `money` into currency `to` with the latest rates on or before `on`,
    /// respecting both currencies' minor-unit exponents. Computed exactly through
    /// EUR and rounded once to the nearest minor unit of `to`, halves away from zero.
    ///
    /// Same currency → unchanged (even if the ECB does not cover it). `None` if
    /// either currency has no rate on or before `on` (not covered by the ECB, or
    /// `on` predates the stored history) or on overflow.
    pub fn convert(&self, money: &Money, to: &str, on: NaiveDate) -> Option<Money> {
        let to = to.to_ascii_uppercase();
        if money.currency.eq_ignore_ascii_case(&to) {
            return Some(Money {
                amount_minor: money.amount_minor,
                currency: to,
            });
        }
        let from_rate = self.rate_on(&money.currency, on)?;
        let to_rate = self.rate_on(&to, on)?;
        // amount_to = amount / 10^exp_from / from_rate * to_rate * 10^exp_to, with
        // rate = mantissa / 10^scale.
        let pow = |e: u32| 10_i128.checked_pow(e);
        let numerator = i128::from(money.amount_minor)
            .checked_mul(i128::from(to_rate.mantissa))?
            .checked_mul(pow(from_rate.scale)?)?
            .checked_mul(pow(minor_unit_exponent(&to))?)?;
        let denominator = i128::from(from_rate.mantissa)
            .checked_mul(pow(to_rate.scale)?)?
            .checked_mul(pow(minor_unit_exponent(&money.currency))?)?;
        Some(Money {
            amount_minor: i64::try_from(div_round(numerator, denominator)).ok()?,
            currency: to,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAILY: &str = include_str!("../fixtures/eurofxref-daily.xml");
    const HIST: &str = include_str!("../fixtures/eurofxref-hist.xml");

    fn date(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    fn money(amount_minor: i64, currency: &str) -> Money {
        Money {
            amount_minor,
            currency: currency.into(),
        }
    }

    fn rates(xml: &str) -> FxRates {
        let mut fx = FxRates::default();
        for day in parse_ecb_xml(xml).unwrap() {
            for (currency, rate) in &day.rates {
                fx.insert(day.date, currency, *rate);
            }
        }
        fx
    }

    #[test]
    fn per_eur_round_trips() {
        for s in ["1.1355", "178.41", "137", "0.85718", "20350.09", "0.0001"] {
            assert_eq!(PerEur::parse(s).unwrap().to_string(), s);
        }
        for bad in [
            "",
            ".5",
            "0",
            "0.000",
            "-1.2",
            "1e3",
            "1.2.3",
            " 1",
            "1234567890123456789",
        ] {
            assert_eq!(PerEur::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn parses_daily_file() {
        let days = parse_ecb_xml(DAILY).unwrap();
        assert_eq!(days.len(), 1);
        assert_eq!(days[0].date, date("2026-09-29"));
        assert_eq!(days[0].rates.len(), 5);
        assert_eq!(
            days[0].rates[0],
            ("USD".into(), PerEur::parse("1.1355").unwrap())
        );
    }

    #[test]
    fn parses_history_file() {
        let days = parse_ecb_xml(HIST).unwrap();
        assert_eq!(
            days.iter().map(|d| d.date).collect::<Vec<_>>(),
            [date("2026-09-29"), date("2026-09-25")]
        );
        assert_eq!(
            days[1].rates[1],
            ("JPY".into(), PerEur::parse("179.7").unwrap())
        );
    }

    #[test]
    fn rejects_garbage() {
        for bad in [
            "",
            "<html><body>Service unavailable</body></html>",
            "<Cube><Cube time='2026-09-29'></Cube></Cube>",
            "<Cube currency='USD' rate='1.1'/>",
            "<Cube time='yesterday'><Cube currency='USD' rate='1.1'/></Cube>",
            "<Cube time='2026-09-29'><Cube currency='USD' rate='abc'/></Cube>",
            "<Cube time='2026-09-29'><Cube currency='usd' rate='1.1'/></Cube>",
            "<Cube time='2026-09-29'><Cube currency='USD' rate='1.1'/",
        ] {
            assert!(parse_ecb_xml(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn converts_via_eur_cross_rates() {
        let fx = rates(DAILY);
        let on = date("2026-09-29");
        let conv = |amount, from, to| {
            fx.convert(&money(amount, from), to, on)
                .map(|m| m.amount_minor)
        };
        // 9.99 USD / 1.1355 = 8.7978 EUR
        assert_eq!(conv(999, "USD", "EUR"), Some(880));
        // 4.99 EUR * 178.41 = 890.27 JPY (0 decimals)
        assert_eq!(conv(499, "EUR", "JPY"), Some(890));
        // 1000 JPY / 178.41 = 5.6051 EUR
        assert_eq!(conv(1000, "JPY", "EUR"), Some(561));
        // 9.99 USD / 1.1355 * 178.41 = 1569.61 JPY
        assert_eq!(conv(999, "USD", "JPY"), Some(1570));
        // 1000 JPY / 178.41 * 1.1355 = 6.3646 USD
        assert_eq!(conv(1000, "JPY", "USD"), Some(636));
        // 9.99 GBP / 0.85718 * 1.1355 = 13.2338 USD
        assert_eq!(conv(999, "GBP", "USD"), Some(1323));
        // ISK has 0 decimals in ISO 4217: 1000 ISK / 136.80 = 7.3099 EUR
        assert_eq!(conv(1000, "ISK", "EUR"), Some(731));
        assert_eq!(conv(-999, "USD", "EUR"), Some(-880));
        assert_eq!(conv(999, "usd", "eur"), Some(880));
        // Identity, even for currencies the ECB does not cover.
        assert_eq!(conv(1234, "EUR", "EUR"), Some(1234));
        assert_eq!(conv(1234, "AED", "AED"), Some(1234));
        // Not covered by the ECB.
        for (from, to) in [
            ("AED", "USD"),
            ("USD", "SAR"),
            ("TWD", "EUR"),
            ("KWD", "EUR"),
        ] {
            assert_eq!(conv(1000, from, to), None, "{from} → {to}");
        }
        assert_eq!(conv(i64::MAX, "EUR", "IDR"), None);
    }

    #[test]
    fn uses_latest_rate_on_or_before_date() {
        let fx = rates(HIST);
        let usd_in_eur = |on: &str| {
            fx.convert(&money(1_000_000, "USD"), "EUR", date(on))
                .map(|m| m.amount_minor)
        };
        // 10000 USD / 1.1403 (Fri 25th) and / 1.1355 (Tue 29th)
        assert_eq!(usd_in_eur("2026-09-25"), Some(876_962));
        assert_eq!(usd_in_eur("2026-09-27"), Some(876_962)); // weekend → Friday
        assert_eq!(usd_in_eur("2026-09-29"), Some(880_669));
        assert_eq!(usd_in_eur("2026-12-31"), Some(880_669));
        assert_eq!(usd_in_eur("2026-09-24"), None); // before the history
        // EUR needs no rate, but the other side still does.
        assert_eq!(
            fx.convert(&money(100, "EUR"), "EUR", date("2000-01-01")),
            Some(money(100, "EUR"))
        );
    }
}
