//! Optional driver-owned model choices for execution roles and standard work.
use boss_protocol::{EffortLevel, ExecutionKind, TaskKind};

#[derive(Debug, Clone, Copy)]
pub struct WorkTypeModelPolicy {
    /// Default for pool automation when no more specific execution role applies.
    pub pool_fallback_model: &'static str,
    /// Engine-minted revision provenance wins over a generic revision run kind.
    pub revision_models: &'static [(&'static str, &'static str)],
    pub kind_models: &'static [(TaskKind, &'static str)],
    pub execution_models: &'static [(ExecutionKind, &'static str)],
    pub standard_model_for_effort: fn(Option<EffortLevel>) -> &'static str,
}

impl WorkTypeModelPolicy {
    pub fn execution_model(&self, kind: Option<&ExecutionKind>, created_via: Option<&str>) -> Option<&'static str> {
        // Progression and triage describe this run, not the revision it inspects.
        let execution = self
            .execution_models
            .iter()
            .find(|(candidate, _)| Some(candidate) == kind);
        if !matches!(
            kind,
            Some(
                ExecutionKind::RevisionImplementation
                    | ExecutionKind::TaskImplementation
                    | ExecutionKind::ChoreImplementation
            )
        ) {
            return execution.map(|(_, model)| *model);
        }
        self.revision_models
            .iter()
            .find(|(prefix, _)| created_via.is_some_and(|value| value.starts_with(prefix)))
            .map(|(_, model)| *model)
            .or_else(|| execution.map(|(_, model)| *model))
    }
}
