//! Argument parsers for sizes, percentages and CPU lists.

/// Parses `512M`, `1.5G`, `800MB`, `2GiB`, `64K` or plain bytes (binary units).
/// Truncates fractional bytes and rejects counts outside `1..=u64::MAX` after truncation.
pub fn size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let (whole, fraction) = num.split_once('.').unwrap_or((num, ""));
    if (whole.is_empty() && fraction.is_empty()) || !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("invalid size: {s}"));
    }
    let multiplier: u64 = match unit.trim().to_ascii_uppercase().as_str() {
        "" | "B" => 1,
        "K" | "KB" | "KIB" => 1 << 10,
        "M" | "MB" | "MIB" => 1 << 20,
        "G" | "GB" | "GIB" => 1 << 30,
        "T" | "TB" | "TIB" => 1 << 40,
        other => return Err(format!("unknown size unit: {other}")),
    };
    const OVERFLOW: &str = "size exceeds the maximum byte count";
    let whole = if whole.is_empty() {
        0
    } else {
        whole.parse::<u64>().map_err(|_| OVERFLOW)?
    };
    // Multiply the decimal fraction right to left, retaining only the carry.
    // The final carry is the whole-byte part, even for long fractions.
    let fractional_bytes = fraction.bytes().rev().fold(0, |carry, digit| {
        (u64::from(digit - b'0') * multiplier + carry) / 10
    });
    let bytes = whole
        .checked_mul(multiplier)
        .and_then(|bytes| bytes.checked_add(fractional_bytes))
        .ok_or(OVERFLOW)?;
    if bytes == 0 {
        return Err("size must be at least 1 byte".into());
    }
    Ok(bytes)
}

/// Parses `20` or `20%`; must be in (0, 100].
pub fn percent(s: &str) -> Result<f32, String> {
    let v: f32 = s
        .trim()
        .trim_end_matches('%')
        .parse()
        .map_err(|_| format!("invalid percentage: {s}"))?;
    if v > 0.0 && v <= 100.0 {
        Ok(v)
    } else {
        Err("percentage must be greater than 0 and at most 100".into())
    }
}

/// Parses `0-3,6` or a hex mask like `0x4F`.
pub fn cpu_list(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        let mask = u64::from_str_radix(hex, 16).map_err(|_| format!("invalid mask: {s}"))?;
        return nonzero(mask);
    }
    let mut mask = 0u64;
    for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (lo, hi) = match part.split_once('-') {
            Some((a, b)) => (cpu_index(a)?, cpu_index(b)?),
            None => (cpu_index(part)?, cpu_index(part)?),
        };
        if lo > hi {
            return Err(format!("invalid range: {part}"));
        }
        for i in lo..=hi {
            mask |= 1 << i;
        }
    }
    nonzero(mask)
}

fn cpu_index(s: &str) -> Result<u32, String> {
    let i: u32 = s
        .trim()
        .parse()
        .map_err(|_| format!("invalid CPU index: {s}"))?;
    if i < 64 {
        Ok(i)
    } else {
        Err(format!("CPU index {i} out of range (0-63)"))
    }
}

fn nonzero(mask: u64) -> Result<u64, String> {
    if mask == 0 {
        Err("CPU list selects no processors".into())
    } else {
        Ok(mask)
    }
}

/// Formats a mask as `0-3,6`.
pub fn format_cpu_list(mask: u64) -> String {
    let mut parts = Vec::new();
    let mut i = 0;
    while i < 64 {
        if mask & (1 << i) == 0 {
            i += 1;
            continue;
        }
        let start = i;
        while i + 1 < 64 && mask & (1 << (i + 1)) != 0 {
            i += 1;
        }
        parts.push(if start == i {
            start.to_string()
        } else {
            format!("{start}-{i}")
        });
        i += 1;
    }
    parts.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sizes() {
        assert_eq!(size("4096"), Ok(4096));
        assert_eq!(size("64K"), Ok(64 * 1024));
        assert_eq!(size("512MB"), Ok(512 << 20));
        assert_eq!(size("1.5G"), Ok(3 << 29));
        assert_eq!(size("2 GiB"), Ok(2 << 30));
        assert!(size("12X").is_err());
        assert!(size("abc").is_err());
        assert_eq!(size("0"), Err("size must be at least 1 byte".into()));
        assert!(size("0.5").is_err());
    }

    #[test]
    fn parses_sizes_without_rounding_or_saturating() {
        assert_eq!(size("1.5KiB"), Ok(1536));
        assert_eq!(size("9007199254740993"), Ok(9_007_199_254_740_993));
        assert_eq!(size("18446744073709551615"), Ok(u64::MAX));
        assert_eq!(size("16777215.99999999999999999999TiB"), Ok(u64::MAX));
        assert!(size("18446744073709551616").is_err());
        assert!(size("16777216TiB").is_err());
        assert_eq!(size(".1K"), Ok(102));
        assert_eq!(size("1.K"), Ok(1024));
        assert!(size("1.2.3K").is_err());
    }

    #[test]
    fn parses_percentages() {
        assert_eq!(percent("20"), Ok(20.0));
        assert_eq!(percent("12.5%"), Ok(12.5));
        assert!(percent("0").is_err());
        assert!(percent("101").is_err());
    }

    #[test]
    fn parses_and_formats_cpu_lists() {
        assert_eq!(cpu_list("0-3,6"), Ok(0b100_1111));
        assert_eq!(cpu_list("0x4F"), Ok(0x4F));
        assert_eq!(cpu_list("5"), Ok(1 << 5));
        assert!(cpu_list("3-1").is_err());
        assert!(cpu_list("64").is_err());
        assert!(cpu_list("0x0").is_err());
        assert_eq!(format_cpu_list(0b100_1111), "0-3,6");
        assert_eq!(format_cpu_list(u64::MAX), "0-63");
        assert_eq!(format_cpu_list(1 << 63), "63");
    }
}
