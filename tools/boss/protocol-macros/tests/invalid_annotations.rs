//! Compiler rejection tests with Bazel-declared compiler and macro inputs.

use boss_protocol_macros::EventTraversal;
use std::path::PathBuf;
use std::process::Command;

#[test]
fn invalid_declarations_and_incomplete_visitors_fail_compilation() {
    let runfiles = std::env::current_dir().unwrap();
    let rustc = runfiles.join(std::env::var("EVENT_RUSTC").unwrap());
    let macro_path = runfiles.join(std::env::var("EVENT_MACRO").unwrap());
    // Keep the runfiles path: canonicalizing rustc can leave the declared sysroot.
    let sysroot = rustc.parent().unwrap().parent().unwrap();
    let scratch: PathBuf = std::env::temp_dir().join("event-compile-tests");
    std::fs::create_dir_all(&scratch).unwrap();
    let cases = [
        ("enum Event { New }", "every event must select"),
        ("enum Event { #[worker()] New }", "empty worker selection"),
        (
            "enum Event { #[worker(automatic)] New }",
            "expected passthrough or transform",
        ),
        (
            "enum Event { #[worker(passthrough, transform = clean)] New }",
            "duplicate worker selection",
        ),
        (
            "enum Event { #[worker(passthrough)] #[worker(passthrough)] New }",
            "duplicate worker selection",
        ),
        ("enum Event { #[worker(passthrough = true)] New }", "expected `,`"),
        (
            "enum Event { #[worker(transform = \"clean\")] New }",
            "expected identifier",
        ),
        (
            "enum Event { #[worker(transform = clean)] A, #[worker(transform = clean)] B }",
            "duplicate transformation hook",
        ),
        ("struct Event;", "event traversal requires an enum"),
        (
            "enum Event<T> { #[worker(passthrough)] Row(T) }",
            "does not support generics",
        ),
        (
            "#[worker(passthrough)] enum Event { New }",
            "worker selection belongs on a variant",
        ),
        (
            "enum Event { #[worker(passthrough)] Row { #[worker(passthrough)] value: u64 } }",
            "worker selection belongs on a variant",
        ),
        (
            "enum Event { #[worker(transform = clean)] Row { value: String } }
          struct Policy; impl EventWorkerVisitor for Policy {}",
            "not all trait items implemented",
        ),
        (
            "enum Event { #[worker(transform = clean)] Row { value: String } }
          struct Policy; impl EventWorkerVisitor for Policy {
              fn clean(&mut self, fields: (u64,)) -> (u64,) { fields }
          }",
            "incompatible type for trait",
        ),
        // A successful compiler invocation guards against missing macro/toolchain inputs.
        ("enum Event { #[worker(passthrough)] New }", ""),
    ];
    for (index, (declaration, expected)) in cases.iter().enumerate() {
        let source = scratch.join(format!("case_{index}.rs"));
        std::fs::write(
            &source,
            format!("use boss_protocol_macros::EventTraversal;\n#[derive(EventTraversal)]\n{declaration}"),
        )
        .unwrap();
        let output = Command::new(&rustc)
            .args(["--edition=2024", "--crate-type=lib", "--emit=metadata", "--color=never"])
            .arg("--sysroot")
            .arg(sysroot)
            .arg("--extern")
            .arg(format!("boss_protocol_macros={}", macro_path.display()))
            .arg("--out-dir")
            .arg(&scratch)
            .arg(&source)
            .output()
            .expect("execute declared compiler");
        let stderr = String::from_utf8_lossy(&output.stderr);
        if expected.is_empty() {
            assert!(output.status.success(), "positive compiler control failed: {stderr}");
        } else {
            assert!(!output.status.success(), "invalid declaration compiled: {declaration}");
            assert!(
                stderr.contains(expected),
                "{declaration}: expected {expected:?}, got {stderr}"
            );
        }
    }
}

#[derive(Debug, PartialEq, EventTraversal)]
enum Event {
    #[worker(passthrough)]
    Unit,
    #[worker(passthrough)]
    Named { value: String },
    #[worker(passthrough)]
    Tuple(String),
    #[worker(transform = clean)]
    Row { __event_visitor: String, count: u64 },
    #[worker(transform = pair)]
    Pair(String, u64),
    #[worker(transform = empty)]
    Empty,
}

struct Policy(usize);
impl EventWorkerVisitor for Policy {
    fn clean(&mut self, (value, count): (String, u64)) -> (String, u64) {
        self.0 += 1;
        (value.to_uppercase(), count + 1)
    }
    fn pair(&mut self, fields: (String, u64)) -> (String, u64) {
        self.clean(fields)
    }
    fn empty(&mut self, (): ()) {
        self.0 += 1;
    }
}

#[test]
fn labels_and_typed_traversal_preserve_payload_shape() {
    let mut policy = Policy(0);
    for event in [
        Event::Unit,
        Event::Named { value: "keep".into() },
        Event::Tuple("keep".into()),
    ] {
        let label = event.variant_label();
        let expected = format!("{event:?}");
        let after = event.map_for_worker(&mut policy);
        assert_eq!(after.variant_label(), label);
        assert_eq!(format!("{after:?}"), expected);
    }
    assert_eq!(policy.0, 0);
    let row = Event::Row {
        __event_visitor: "hello".into(),
        count: 2,
    };
    assert_eq!(row.variant_label(), "Row");
    assert_eq!(
        row.map_for_worker(&mut policy),
        Event::Row {
            __event_visitor: "HELLO".into(),
            count: 3
        }
    );
    assert_eq!(
        Event::Pair("hello".into(), 2).map_for_worker(&mut policy),
        Event::Pair("HELLO".into(), 3)
    );
    assert_eq!(Event::Empty.map_for_worker(&mut policy), Event::Empty);
    assert_eq!(policy.0, 3);
}
