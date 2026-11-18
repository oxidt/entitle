//! Number, money and date formatting, matching what the browser's `Intl` API
//! produced for `en` when the dashboard rendered client-side.

use chrono::{DateTime, Datelike, NaiveDate, Utc};
use entitle_core::currency::minor_unit_exponent;

/// `Intl.NumberFormat("en", { style: "currency" })` symbols; other currencies
/// are written as their code followed by a no-break space.
fn symbol(currency: &str) -> Option<&'static str> {
    Some(match currency {
        "USD" => "$",
        "EUR" => "€",
        "GBP" => "£",
        "JPY" => "¥",
        "CNY" => "CN¥",
        "INR" => "₹",
        "KRW" => "₩",
        "ILS" => "₪",
        "VND" => "₫",
        "PHP" => "₱",
        "BRL" => "R$",
        "AUD" => "A$",
        "CAD" => "CA$",
        "NZD" => "NZ$",
        "HKD" => "HK$",
        "MXN" => "MX$",
        "TWD" => "NT$",
        "XCD" => "EC$",
        "XAF" => "FCFA",
        "XOF" => "F\u{a0}CFA",
        "XPF" => "CFPF",
        _ => return None,
    })
}

/// Digits with thousands separators: `1234567` → `1,234,567`.
fn grouped(digits: &str) -> String {
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Full money value, e.g. `€12,345.67`, `-$5.00` or `¥12,345`.
pub fn money(minor: i64, currency: &str) -> String {
    let exp = minor_unit_exponent(currency);
    let scale = 10u64.pow(exp);
    let abs = minor.unsigned_abs();
    let mut number = grouped(&(abs / scale).to_string());
    if exp > 0 {
        number.push_str(&format!(".{:0width$}", abs % scale, width = exp as usize));
    }
    let sign = if minor < 0 { "-" } else { "" };
    match symbol(currency) {
        Some(s) => format!("{sign}{s}{number}"),
        None => format!("{sign}{currency}\u{a0}{number}"),
    }
}

/// Whole number with thousands separators.
pub fn int(n: impl Into<i64>) -> String {
    let n = n.into();
    let digits = grouped(&n.unsigned_abs().to_string());
    if n < 0 { format!("-{digits}") } else { digits }
}

/// `0.1234` → `12.3%` (with `digits` decimals).
pub fn percent(ratio: f64, digits: usize) -> String {
    format!("{:.digits$}%", ratio * 100.0)
}

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

fn month_name(d: NaiveDate) -> &'static str {
    MONTHS[d.month0() as usize]
}

/// `Oct 26`: an axis label.
pub fn short_month(d: NaiveDate) -> String {
    format!("{} {:02}", &month_name(d)[..3], d.year() % 100)
}

/// `October 2026`.
pub fn long_month(d: NaiveDate) -> String {
    format!("{} {}", month_name(d), d.year())
}

/// `Oct 12, 2026`.
pub fn full_date(d: NaiveDate) -> String {
    format!("{} {}, {}", &month_name(d)[..3], d.day(), d.year())
}

/// An instant shown in the viewer's time zone: the server writes UTC and the
/// page script rewrites it locally (`data-local`).
pub fn utc_text(t: DateTime<Utc>) -> String {
    t.format("%b %-d, %H:%M:%S UTC").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn money_matches_intl_en() {
        assert_eq!(money(1234567, "EUR"), "€12,345.67");
        assert_eq!(money(-500, "USD"), "-$5.00");
        assert_eq!(money(12345, "JPY"), "¥12,345");
        assert_eq!(money(1234, "KWD"), "KWD\u{a0}1.234");
        assert_eq!(money(5, "CHF"), "CHF\u{a0}0.05");
        assert_eq!(money(0, "EUR"), "€0.00");
    }

    #[test]
    fn numbers_and_dates() {
        assert_eq!(int(1234567u64 as i64), "1,234,567");
        assert_eq!(int(-12), "-12");
        assert_eq!(int(999), "999");
        assert_eq!(percent(0.12345, 1), "12.3%");
        assert_eq!(percent(0.05, 2), "5.00%");
        let d = NaiveDate::from_ymd_opt(2026, 10, 12).unwrap();
        assert_eq!(short_month(d), "Oct 26");
        assert_eq!(long_month(d), "October 2026");
        assert_eq!(full_date(d), "Oct 12, 2026");
    }
}
