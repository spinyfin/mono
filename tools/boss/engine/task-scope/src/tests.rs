use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::oneshot;

struct Dropped(Arc<AtomicBool>);

impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_drains_nested_sleeping_tasks_and_preserves_other_scopes() {
    let child_dropped = Arc::new(AtomicBool::new(false));
    let (started, ready) = oneshot::channel();
    let other = tokio::spawn(std::future::pending::<()>());
    run(async {
        let child_marker = child_dropped.clone();
        spawn(async move {
            spawn(async move {
                let _guard = Dropped(child_marker);
                let _ = started.send(());
                tokio::time::sleep(Duration::from_secs(3600)).await;
                panic!("sleeping task outlived its engine");
            });
        });
        ready.await.unwrap();
        shutdown().await;
        assert!(child_dropped.load(Ordering::SeqCst));
        let forbidden = spawn(async { panic!("work started after shutdown") });
        assert!(forbidden.await.unwrap_err().is_cancelled());
    })
    .await;
    assert!(!other.is_finished(), "another engine's work must be untouched");
    other.abort();
    assert!(other.await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn error_return_drains_tasks_and_finished_handles_keep_tokio_semantics() {
    let dropped = Arc::new(AtomicBool::new(false));
    let result: Result<(), ()> = run(async {
        assert_eq!(spawn(async { 42 }).await.unwrap(), 42);
        assert!(spawn(async { panic!("test payload") }).await.unwrap_err().is_panic());
        let (started, ready) = oneshot::channel();
        let guard = Dropped(dropped.clone());
        spawn(async move {
            let _guard = guard;
            started.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        ready.await.unwrap();
        Err(())
    })
    .await;
    assert_eq!(result, Err(()));
    assert!(dropped.load(Ordering::SeqCst));
}

#[tokio::test]
async fn dropping_the_owner_cancels_its_children() {
    let (started, ready) = oneshot::channel();
    let owner = tokio::spawn(run(async {
        let child = spawn(std::future::pending::<()>());
        started.send(child).unwrap();
        std::future::pending::<()>().await;
    }));
    let child = ready.await.unwrap();
    owner.abort();
    assert!(owner.await.unwrap_err().is_cancelled());
    assert!(child.await.unwrap_err().is_cancelled());
}

#[test]
fn abrupt_runtime_teardown_cancels_without_holding_the_registry_lock() {
    for multi_thread in [false, true] {
        let (finished, completion) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let mut builder = if multi_thread {
                tokio::runtime::Builder::new_multi_thread()
            } else {
                tokio::runtime::Builder::new_current_thread()
            };
            let runtime = builder.worker_threads(2).enable_all().build().unwrap();
            runtime.block_on(async {
                let (started, ready) = oneshot::channel();
                tokio::spawn(run(async {
                    spawn(std::future::pending::<()>());
                    started.send(()).unwrap();
                    std::future::pending::<()>().await;
                }));
                ready.await.unwrap();
            });
            drop(runtime);
            finished.send(()).unwrap();
        });
        completion
            .recv_timeout(Duration::from_secs(5))
            .expect("runtime teardown deadlocked task cancellation");
        thread.join().unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn join_set_children_are_drained_even_when_the_parent_is_cancelled() {
    let dropped = Arc::new(AtomicBool::new(false));
    run(async {
        let marker = dropped.clone();
        let (started, ready) = oneshot::channel();
        spawn(async move {
            let mut set = JoinSet::new();
            spawn_into(&mut set, async move {
                let _guard = Dropped(marker);
                started.send(()).unwrap();
                tokio::time::sleep(Duration::from_secs(3600)).await;
            });
            set.join_next().await
        });
        ready.await.unwrap();
    })
    .await;
    assert!(dropped.load(Ordering::SeqCst));
}

#[tokio::test]
async fn task_destructors_cannot_escape_shutdown_by_spawning_more_work() {
    struct SpawnOnDrop(Option<oneshot::Sender<JoinHandle<()>>>);
    impl Drop for SpawnOnDrop {
        fn drop(&mut self) {
            let _ = self.0.take().unwrap().send(spawn(async {
                panic!("destructor started work in a closing scope");
            }));
        }
    }
    let (sender, receiver) = oneshot::channel();
    run(async {
        let (started, ready) = oneshot::channel();
        spawn(async move {
            let _guard = SpawnOnDrop(Some(sender));
            started.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        ready.await.unwrap();
    })
    .await;
    assert!(receiver.await.unwrap().await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn a_panicking_destructor_does_not_strand_shutdown() {
    struct PanicOnDrop;
    impl Drop for PanicOnDrop {
        fn drop(&mut self) {
            panic!("test destructor panic");
        }
    }
    let mut child = None;
    run(async {
        let (started, ready) = oneshot::channel();
        child = Some(spawn(async move {
            let _guard = PanicOnDrop;
            started.send(()).unwrap();
            std::future::pending::<()>().await;
        }));
        ready.await.unwrap();
    })
    .await;
    assert!(child.unwrap().await.unwrap_err().is_panic());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocking_read_handlers_cancel_their_timers_and_inherit_child_ownership() {
    let dropped = Arc::new(AtomicBool::new(false));
    let mut handle = None;
    run(async {
        let (started, ready) = oneshot::channel();
        let marker = dropped.clone();
        handle = Some(spawn_blocking_future(async move {
            spawn(async move {
                let _guard = Dropped(marker);
                started.send(()).unwrap();
                tokio::time::sleep(Duration::from_secs(3600)).await;
            });
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }));
        ready.await.unwrap();
    })
    .await;
    assert_eq!(handle.unwrap().await.unwrap(), None);
    assert!(dropped.load(Ordering::SeqCst));
}
