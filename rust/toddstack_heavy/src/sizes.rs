//! Byte sizes: parse `4G`, `512M`, `1.5G`; print them back.

use crate::error::Error;

const KIB: u128 = 1024;

/// Parses a size with an optional K/M/G/T suffix (binary, optional `B` or `iB`). No suffix means bytes.
///
/// # Errors
/// Returns a usage error when the text is not a size.
pub fn parse_size(text: &str) -> Result<u64, Error> {
    let bad = || Error::Usage(format!("not a size: {text:?} (try 512M or 4G)"));
    let t = text.trim();
    let split = t
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(t.len());
    let (num, suffix) = t.split_at(split);
    let unit: u128 = match suffix
        .to_ascii_lowercase()
        .trim_end_matches('b')
        .trim_end_matches('i')
    {
        "" => 1,
        "k" => KIB,
        "m" => KIB.pow(2),
        "g" => KIB.pow(3),
        "t" => KIB.pow(4),
        _ => return Err(bad()),
    };
    let (whole, frac) = num.split_once('.').unwrap_or((num, ""));
    if whole.is_empty() && frac.is_empty() {
        return Err(bad());
    }
    let whole: u128 = if whole.is_empty() {
        0
    } else {
        whole.parse().map_err(|_| bad())?
    };
    let mut total = whole.checked_mul(unit).ok_or_else(bad)?;
    if !frac.is_empty() {
        let digits = frac.get(..frac.len().min(9)).ok_or_else(bad)?;
        let f: u128 = digits.parse().map_err(|_| bad())?;
        let scale = 10_u128.pow(u32::try_from(digits.len()).map_err(|_| bad())?);
        total = total.checked_add(f * unit / scale).ok_or_else(bad)?;
    }
    u64::try_from(total).map_err(|_| bad())
}

/// Human form with one decimal, binary units.
#[must_use]
pub fn fmt_size(bytes: u64) -> String {
    let units = ["B", "K", "M", "G", "T"];
    let mut idx = 0;
    let mut scaled = u128::from(bytes) * 10;
    while scaled >= 10240 && idx < units.len() - 1 {
        scaled /= 1024;
        idx += 1;
    }
    let unit = units.get(idx).copied().unwrap_or("T");
    if idx == 0 {
        format!("{bytes}B")
    } else {
        format!("{}.{}{unit}", scaled / 10, scaled % 10)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_units() {
        assert_eq!(parse_size("4G").unwrap(), 4 << 30);
        assert_eq!(parse_size("512M").unwrap(), 512 << 20);
        assert_eq!(parse_size("1.5G").unwrap(), 3 << 29);
        assert_eq!(parse_size("100").unwrap(), 100);
        assert_eq!(parse_size("2GiB").unwrap(), 2 << 30);
        assert!(parse_size("fast").is_err());
        assert!(parse_size("").is_err());
        assert!(parse_size("4X").is_err());
    }

    #[test]
    fn formats() {
        assert_eq!(fmt_size(512), "512B");
        assert_eq!(fmt_size(3 << 29), "1.5G");
        assert_eq!(fmt_size(10 << 20), "10.0M");
    }
}
