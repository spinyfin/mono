//! Compact human duration parsing (`30s`, `5m`, `2h`, `1d`, `1w`, `1h30m`).

/// Parse a compact duration into whole seconds.
///
/// Accepts one or more `<digits><unit>` terms with unit `s`, `m`, `h`, `d`
/// or `w` (case-insensitive), summed, e.g. `90s`, `2h`, `1h30m`. A bare
/// number without a unit is rejected; callers that want bare seconds
/// handle that case themselves. Arithmetic saturates rather than
/// overflowing. A zero total is returned as `Ok(0)`; range checks belong
/// to the caller.
pub fn parse_compact_duration_secs(raw: &str) -> Result<u64, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("duration must not be empty".to_owned());
    }
    let bytes = trimmed.as_bytes();
    let mut i = 0;
    let mut total: u64 = 0;
    while i < bytes.len() {
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if start == i {
            return Err(format!(
                "duration `{raw}` is not a compact duration like 30s, 5m, 2h, 1d or 1h30m"
            ));
        }
        let n: u64 = trimmed[start..i]
            .parse()
            .map_err(|_| format!("duration `{raw}` contains a number that does not fit in 64 bits"))?;
        let Some(&unit) = bytes.get(i) else {
            return Err(format!("duration `{raw}` is missing a unit (s, m, h, d or w)"));
        };
        i += 1;
        let multiplier = match unit.to_ascii_lowercase() {
            b's' => 1_u64,
            b'm' => 60,
            b'h' => 3_600,
            b'd' => 86_400,
            b'w' => 604_800,
            _ => {
                return Err(format!(
                    "unknown duration unit `{}` in `{raw}`; expected s, m, h, d or w",
                    unit as char
                ));
            }
        };
        total = total.saturating_add(n.saturating_mul(multiplier));
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_units_and_compounds() {
        assert_eq!(parse_compact_duration_secs("30s").unwrap(), 30);
        assert_eq!(parse_compact_duration_secs("5m").unwrap(), 300);
        assert_eq!(parse_compact_duration_secs("2H").unwrap(), 7_200);
        assert_eq!(parse_compact_duration_secs("1d").unwrap(), 86_400);
        assert_eq!(parse_compact_duration_secs("2w").unwrap(), 1_209_600);
        assert_eq!(parse_compact_duration_secs("1h30m").unwrap(), 5_400);
        assert_eq!(parse_compact_duration_secs("0h").unwrap(), 0);
    }

    #[test]
    fn rejects_malformed_input() {
        assert!(parse_compact_duration_secs("").is_err());
        assert!(parse_compact_duration_secs("90").is_err());
        assert!(parse_compact_duration_secs("h").is_err());
        assert!(parse_compact_duration_secs("5x").is_err());
        assert!(parse_compact_duration_secs("1h-5m").is_err());
    }

    #[test]
    fn saturates_instead_of_overflowing() {
        assert_eq!(parse_compact_duration_secs("18446744073709551615w").unwrap(), u64::MAX);
    }
}
