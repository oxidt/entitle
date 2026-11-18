//! ISO 4217 minor units, for converting store price formats into [`Money::amount_minor`].
//!
//! [`Money::amount_minor`]: crate::Money

/// Currencies whose minor unit is not 2 decimals (ISO 4217).
const EXPONENTS: &[(&str, u32)] = &[
    ("BIF", 0),
    ("CLP", 0),
    ("DJF", 0),
    ("GNF", 0),
    ("ISK", 0),
    ("JPY", 0),
    ("KMF", 0),
    ("KRW", 0),
    ("PYG", 0),
    ("RWF", 0),
    ("UGX", 0),
    ("UYI", 0),
    ("VND", 0),
    ("VUV", 0),
    ("XAF", 0),
    ("XOF", 0),
    ("XPF", 0),
    ("BHD", 3),
    ("IQD", 3),
    ("JOD", 3),
    ("KWD", 3),
    ("LYD", 3),
    ("OMR", 3),
    ("TND", 3),
    ("CLF", 4),
    ("UYW", 4),
];

/// Number of decimals of `currency`'s minor unit (2 unless listed as an exception).
pub fn minor_unit_exponent(currency: &str) -> u32 {
    EXPONENTS
        .iter()
        .find(|(code, _)| code.eq_ignore_ascii_case(currency))
        .map_or(2, |(_, exp)| *exp)
}

/// Apple milliunits (1/1000 of the major unit) → minor units. Truncates sub-minor
/// precision; `None` on overflow.
pub fn minor_from_milliunits(milliunits: i64, currency: &str) -> Option<i64> {
    let exp = minor_unit_exponent(currency);
    if exp <= 3 {
        Some(milliunits / 10_i64.pow(3 - exp))
    } else {
        milliunits.checked_mul(10_i64.pow(exp - 3))
    }
}

/// `google.type.Money` (`units` + `nanos`, 1e-9 of the major unit, same sign) → minor
/// units. Truncates sub-minor precision; `None` on overflow.
pub fn minor_from_units_nanos(units: i64, nanos: i64, currency: &str) -> Option<i64> {
    let exp = minor_unit_exponent(currency);
    units
        .checked_mul(10_i64.pow(exp))?
        .checked_add(nanos / 10_i64.pow(9 - exp))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exponents() {
        assert_eq!(minor_unit_exponent("EUR"), 2);
        assert_eq!(minor_unit_exponent("JPY"), 0);
        assert_eq!(minor_unit_exponent("jpy"), 0);
        assert_eq!(minor_unit_exponent("KWD"), 3);
        assert_eq!(minor_unit_exponent("CLF"), 4);
    }

    #[test]
    fn milliunits() {
        assert_eq!(minor_from_milliunits(4990, "EUR"), Some(499));
        assert_eq!(minor_from_milliunits(480_000, "JPY"), Some(480));
        assert_eq!(minor_from_milliunits(1_250, "KWD"), Some(1_250));
        assert_eq!(minor_from_milliunits(1_250, "CLF"), Some(12_500));
        assert_eq!(minor_from_milliunits(i64::MAX, "CLF"), None);
    }

    #[test]
    fn units_nanos() {
        assert_eq!(minor_from_units_nanos(4, 990_000_000, "EUR"), Some(499));
        assert_eq!(minor_from_units_nanos(480, 0, "JPY"), Some(480));
        assert_eq!(minor_from_units_nanos(1, 250_000_000, "KWD"), Some(1_250));
        assert_eq!(minor_from_units_nanos(-4, -990_000_000, "EUR"), Some(-499));
        assert_eq!(minor_from_units_nanos(i64::MAX, 0, "EUR"), None);
    }
}
