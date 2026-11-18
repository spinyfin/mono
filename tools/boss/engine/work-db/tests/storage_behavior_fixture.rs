//! Minimal public WorkDb fixture for storage-behavior tests.
//!
//! Seeds the `local` host with the `driver=claude` auto capability through
//! public [`WorkDb::open`] and [`WorkDb::replace_auto_host_capabilities`], so
//! dispatch-dependent storage tests need no installed agent CLIs. Production
//! code does not depend on this library.

use tempfile::TempDir;

use boss_protocol::{CreateExecutionInput, ExecutionKind, ExecutionStatus, Product, Task, WorkExecution};
use boss_work_db::{CreateChoreInput, CreateProductInput, WorkDb};

/// Repo remote used by [`create_test_product`].
pub const TEST_REPO_REMOTE_URL: &str = "git@github.com:spinyfin/mono.git";

/// Auto-sourced capability the local host advertises in these tests.
pub const LOCAL_HOST_ID: &str = "local";
pub const DRIVER_CLAUDE_CAPABILITY: &str = "driver=claude";

/// Open a fresh file-backed [`WorkDb`] and seed the local driver capability.
///
/// The returned `TempDir` must be kept alive for the lifetime of the `WorkDb`.
pub fn open_db() -> (TempDir, WorkDb) {
    let dir = TempDir::new().unwrap();
    let db = WorkDb::open(dir.path().join("state.db")).unwrap();
    ensure_local_driver_capability(&db);
    (dir, db)
}

/// Replace auto-sourced capabilities on `local` with `driver=claude`.
///
/// `WorkDb::open` already inserts the `local` host row, so this does not
/// need raw SQL or a separate host insert.
pub fn ensure_local_driver_capability(db: &WorkDb) {
    db.replace_auto_host_capabilities(LOCAL_HOST_ID, &[DRIVER_CLAUDE_CAPABILITY.to_owned()])
        .expect("local host exists after WorkDb::open");
}

/// Create the standard test product: name `Boss`, the mono repo remote.
pub fn create_test_product(db: &WorkDb) -> Product {
    db.create_product(
        CreateProductInput::builder()
            .name("Boss")
            .repo_remote_url(TEST_REPO_REMOTE_URL)
            .build(),
    )
    .unwrap()
}

/// Create a plain chore named `name` under `product_id` and return it.
pub fn create_test_chore(db: &WorkDb, product_id: impl Into<String>, name: impl Into<String>) -> Task {
    db.create_chore(CreateChoreInput::builder().product_id(product_id).name(name).build())
        .unwrap()
}

/// Create a `ready` [`ExecutionKind::ChoreImplementation`] execution.
pub fn create_ready_chore_execution(db: &WorkDb, work_item_id: impl Into<String>) -> WorkExecution {
    db.create_execution(
        CreateExecutionInput::builder()
            .work_item_id(work_item_id)
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::Ready)
            .build(),
    )
    .unwrap()
}
