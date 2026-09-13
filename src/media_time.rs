/// Parse an MPD/UPnP clock without allowing malformed fields or integer
/// overflow from an untrusted network response.
pub fn clock_to_seconds(value: Option<&str>) -> Option<f64> {
    let value = value?.trim();
    if value.is_empty() || value == "NOT_IMPLEMENTED" {
        return None;
    }
    let parts = value
        .split(':')
        .map(str::parse::<u64>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    let total = match parts.as_slice() {
        [minutes, seconds] if *seconds < 60 => minutes
            .checked_mul(60)
            .and_then(|value| value.checked_add(*seconds)),
        [hours, minutes, seconds] if *minutes < 60 && *seconds < 60 => hours
            .checked_mul(3_600)
            .and_then(|value| value.checked_add(minutes * 60))
            .and_then(|value| value.checked_add(*seconds)),
        _ => None,
    }?;
    Some(total as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_clock_formats() {
        assert_eq!(clock_to_seconds(Some("02:03")), Some(123.0));
        assert_eq!(clock_to_seconds(Some("01:02:03")), Some(3_723.0));
    }

    #[test]
    fn rejects_invalid_or_overflowing_clocks() {
        assert_eq!(clock_to_seconds(Some("00:99:00")), None);
        assert_eq!(clock_to_seconds(Some("1:bad")), None);
        assert_eq!(clock_to_seconds(Some("18446744073709551615:00")), None);
        assert_eq!(clock_to_seconds(Some("NOT_IMPLEMENTED")), None);
    }
}
