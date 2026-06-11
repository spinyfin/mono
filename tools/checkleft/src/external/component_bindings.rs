// Host-side bindings generated from the `checkleft:check@0.1.0` WIT package.
//
// `v1` covers `world check` (the original two-export interface). `v2` covers
// `world check-with-exclusion-audit` which adds `list-exclusions` and
// `evaluate-exclusion`. The host uses `v1` for normal check execution (backward
// compatible with all components) and attempts `v2` only for the stale-exclusion
// audit path. Components that only implement `world check` fail the `v2`
// instantiation; the host catches that and skips the audit (fail-safe).
pub mod v1 {
    wasmtime::component::bindgen!({
        world: "check",
        path: "wit/check.wit",
    });
}

pub mod v2 {
    wasmtime::component::bindgen!({
        world: "check-with-exclusion-audit",
        path: "wit/check.wit",
    });
}

#[cfg(test)]
mod tests {
    use super::v1::checkleft::check::types;

    // Verify that the v1 generated types are usable: construct a minimal
    // `CheckDescriptor` and round-trip a `CheckError`.

    #[test]
    fn check_descriptor_can_be_constructed() {
        let desc = types::CheckDescriptor {
            name: "example-check".to_owned(),
            description: "An example check for smoke testing.".to_owned(),
            default_severity: types::Severity::Warning,
            access_scope: None,
        };
        assert_eq!(desc.name, "example-check");
        assert_eq!(desc.default_severity, types::Severity::Warning);
        assert!(desc.access_scope.is_none());
    }

    #[test]
    fn access_scope_variants_are_generated() {
        let _modified_only = types::AccessScope::ModifiedOnly;
        let _whole_repo = types::AccessScope::WholeRepo;
        let _globs = types::AccessScope::Globs(vec!["**/Cargo.toml".to_owned()]);
    }

    #[test]
    fn check_error_variants_are_generated() {
        let unknown = types::CheckError::UnknownCheck("no-such-check".to_owned());
        let failed = types::CheckError::Failed("something went wrong".to_owned());
        match unknown {
            types::CheckError::UnknownCheck(name) => assert_eq!(name, "no-such-check"),
            types::CheckError::Failed(_) => panic!("unexpected variant"),
        }
        match failed {
            types::CheckError::Failed(msg) => assert_eq!(msg, "something went wrong"),
            types::CheckError::UnknownCheck(_) => panic!("unexpected variant"),
        }
    }

    #[test]
    fn finding_can_be_constructed() {
        let finding = types::Finding {
            severity: types::Severity::Error,
            message: "something is wrong".to_owned(),
            location: Some(types::Location {
                path: "src/lib.rs".to_owned(),
                line: Some(42),
                column: None,
            }),
            remediations: vec!["fix it".to_owned()],
            suggested_fix: None,
        };
        assert_eq!(finding.severity, types::Severity::Error);
        assert_eq!(finding.location.as_ref().unwrap().line, Some(42));
    }

    #[test]
    fn v2_declared_exclusion_type_is_generated() {
        use super::v2::checkleft::check::types as v2_types;
        let excl = v2_types::DeclaredExclusion {
            entry: "engine/src/app.rs::ServerState".to_owned(),
            depends_on: vec!["engine/src/app.rs".to_owned()],
        };
        assert_eq!(excl.entry, "engine/src/app.rs::ServerState");
        assert_eq!(excl.depends_on.len(), 1);
    }

    #[test]
    fn v2_exclusion_status_variants_are_generated() {
        use super::v2::checkleft::check::types as v2_types;
        let lb = v2_types::ExclusionStatus::LoadBearing;
        let stale = v2_types::ExclusionStatus::Stale("no longer needed".to_owned());
        let unknown = v2_types::ExclusionStatus::Unknown;
        let _ = (lb, stale, unknown);
    }
}
