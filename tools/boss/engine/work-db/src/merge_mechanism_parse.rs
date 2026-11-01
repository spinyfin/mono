//! Validate a product `merge_mechanism` column at the storage write path.
use anyhow::{Result, bail};

/// Accept `None` / `"direct"` / `"trunk_queue"`; reject any other value.
pub fn parse(raw: Option<&str>) -> Result<()> {
    match raw {
        None | Some("direct") | Some("trunk_queue") => Ok(()),
        Some(other) => {
            bail!("unknown products.merge_mechanism value `{other}`; expected one of: direct, trunk_queue")
        }
    }
}
