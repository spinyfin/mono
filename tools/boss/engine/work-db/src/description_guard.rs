//! Validate description replacements at the transactional write boundary.

use anyhow::{Result, bail};

pub(super) fn validate_description_update(old: &str, new: &str, force_shrink: bool) -> Result<()> {
    let trimmed = new.trim();
    if ["null", "undefined", "none"]
        .iter()
        .any(|value| trimmed.eq_ignore_ascii_case(value))
    {
        bail!(
            "description cannot be the placeholder {trimmed:?}; provide a real brief or clear it with --description \"\" --force-shrink"
        );
    }
    if !force_shrink {
        if trimmed.is_empty() {
            bail!(
                "description cannot be blank on update ({} old bytes, {} new bytes); to clear it deliberately use --description \"\" --force-shrink",
                old.len(),
                new.len()
            );
        }
        if old.len() >= 500 && new.len().saturating_mul(10) < old.len() {
            bail!(
                "description shrinks from {} bytes to {} bytes (less than 10%); use --force-shrink to confirm",
                old.len(),
                new.len()
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shrink_threshold_uses_bytes_and_strict_ten_percent_boundary() {
        let old = "é".repeat(250);
        assert!(validate_description_update(&old, &"x".repeat(49), false).is_err());
        assert!(validate_description_update(&old, &"x".repeat(50), false).is_ok());
        assert!(validate_description_update(&"x".repeat(499), "short", false).is_ok());
        assert!(validate_description_update(&"x".repeat(501), &"x".repeat(50), false).is_err());
        assert!(validate_description_update(&old, "short", true).is_ok());
        assert!(validate_description_update(&old, "", true).is_ok());
    }

    #[test]
    fn override_defaults_off_for_existing_rpc_clients() {
        let patch: boss_protocol::WorkItemPatch = serde_json::from_str(r#"{"description":"short"}"#).unwrap();
        assert!(!patch.force_shrink);
        assert!(
            !boss_protocol::WorkItemPatch::builder()
                .description("short")
                .build()
                .force_shrink
        );
    }
}
