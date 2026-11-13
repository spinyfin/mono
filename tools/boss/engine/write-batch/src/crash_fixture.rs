//! Child process used only by the Bazel crash test. Never opens engine state.
use boss_write_batch::Writer;
use rusqlite::Connection;
use std::sync::{Arc, Mutex};

fn main() {
    let args: Vec<_> = std::env::args().collect();
    let c = Connection::open(&args[1]).unwrap();
    c.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
        CREATE TABLE IF NOT EXISTS receipt(sequence INTEGER PRIMARY KEY);
        CREATE TABLE IF NOT EXISTS frames(sequence INTEGER PRIMARY KEY, bytes BLOB);",
    )
    .unwrap();
    let writer = Writer::new(Arc::new(Mutex::new(c))).unwrap();
    let before = args[2] == "before";
    writer
        .submit("host".into(), 128, true, move |tx| {
            tx.execute("INSERT INTO frames VALUES (1, X'010203')", [])?;
            tx.execute("INSERT INTO receipt VALUES (1)", [])?;
            if before {
                std::process::exit(23);
            }
            Ok(())
        })
        .unwrap()
        .wait()
        .unwrap();
    std::process::exit(24);
}
