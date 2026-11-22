pub(super) fn format_bytes(value: f64) -> String {
    let round = |value: f64| {
        let lower = value.floor();
        lower + if value - lower >= 0.5 { 1.0 } else { 0.0 }
    };
    let bytes = round(value);
    if bytes < 1024.0 {
        return format!("{} B", if bytes == 0.0 { 0.0 } else { bytes });
    }
    let mut scaled = value / 1024.0;
    for unit in ["KB", "MB", "GB", "TB"] {
        let (factor, precision) = if scaled >= 100.0 {
            (1.0, 0)
        } else if scaled >= 10.0 {
            (10.0, 1)
        } else {
            (100.0, 2)
        };
        let lower = (scaled * factor).floor();
        let rounded = (lower
            + if scaled.mul_add(factor, -lower) >= 0.5 {
                1.0
            } else {
                0.0
            })
            / factor;
        if rounded < 1024.0 || unit == "TB" {
            return format!("{rounded:.precision$} {unit}");
        }
        scaled /= 1024.0;
    }
    unreachable!("the final unit always formats the value")
}

pub(super) fn group_number(value: f64) -> String {
    let value = format!("{value:.3}");
    let (whole, fraction) = value.split_once('.').unwrap_or((&value, ""));
    let digits = whole.strip_prefix('-').unwrap_or(whole);
    let mut result = String::new();
    if whole.starts_with('-') {
        result.push('-');
    }
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            result.push(',');
        }
        result.push(digit);
    }
    let fraction = fraction.trim_end_matches('0');
    if !fraction.is_empty() {
        result.push('.');
        result.push_str(fraction);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovered_size_rounds_before_promoting_units() {
        assert_eq!(format_bytes(0.0), "0 B");
        assert_eq!(format_bytes(1050.0), "1.03 KB");
        assert_eq!(format_bytes(12_544.0), "12.3 KB");
        assert_eq!(format_bytes(1_048_320.0), "1.00 MB");
        assert_eq!(group_number(12345.25), "12,345.25");
    }
}
