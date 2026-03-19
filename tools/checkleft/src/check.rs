use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Result, bail};
use async_trait::async_trait;

use crate::input::{ChangeSet, SourceTree};
use crate::output::CheckResult;

#[async_trait]
pub trait ConfiguredCheck: Send + Sync {
    async fn run(&self, changeset: &ChangeSet, tree: &dyn SourceTree) -> Result<CheckResult>;
}

#[async_trait]
pub trait Check: Send + Sync {
    fn id(&self) -> &str;

    fn description(&self) -> &str;

    fn configure(&self, config: &toml::Value) -> Result<Arc<dyn ConfiguredCheck>>;

    async fn run(
        &self,
        changeset: &ChangeSet,
        tree: &dyn SourceTree,
        config: &toml::Value,
    ) -> Result<CheckResult> {
        self.configure(config)?.run(changeset, tree).await
    }
}

#[derive(Default)]
pub struct CheckRegistry {
    checks: BTreeMap<String, Arc<dyn Check>>,
}

impl CheckRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register<C>(&mut self, check: C) -> Result<()>
    where
        C: Check + 'static,
    {
        self.register_arc(Arc::new(check))
    }

    pub fn register_arc(&mut self, check: Arc<dyn Check>) -> Result<()> {
        let id = check.id().to_owned();
        if self.checks.contains_key(&id) {
            bail!("check already registered: {id}");
        }
        self.checks.insert(id, check);
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn Check>> {
        self.checks.get(id).cloned()
    }

    pub fn list(&self) -> Vec<Arc<dyn Check>> {
        self.checks.values().cloned().collect()
    }
}
