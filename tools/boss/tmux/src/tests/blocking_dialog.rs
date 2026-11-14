use super::*;

const MARKER: &str = "Do you want to proceed?";
const DIALOG: &str = "Do you want to proceed?\n❯ 1. Yes\n  2. No\nEsc to cancel";

#[tokio::test]
async fn live_claude_trust_layout_is_blocked_but_transcript_copy_is_clear() {
    // Captured from isolated Claude Code 2.1.325: these choices have no numbers.
    for (screen, marker) in [
        (
            "Accessing workspace:\nQuick safety check: Is this a project you created or one you trust?\n\n❯ No, exit\n  Yes, I trust this folder\n\nEnter to confirm · Esc to cancel",
            "Yes, I trust this folder",
        ),
        (
            "WARNING: Claude Code running in Bypass Permissions mode\n❯ No, exit\n  Yes, I accept\nEnter to confirm · Esc to cancel",
            "Yes, I accept",
        ),
    ] {
        let (tmux, _) = tmux([success(screen)]);
        let error = tmux
            .with_blocking_dialog_markers(vec![marker.into()])
            .send_submit("worker")
            .await
            .unwrap_err();
        assert!(error.is::<BlockingDialogOnScreen>());
    }
    let transcript = format!("{DIALOG}\nClaude Code\n❯ status?");
    let (tmux, _) = super::tmux([success(&transcript), success("")]);
    tmux.with_blocking_dialog_markers(vec![MARKER.into()])
        .send_submit("worker")
        .await
        .unwrap();
}

#[tokio::test]
async fn dialog_phrases_in_payload_and_transcript_are_delivered() {
    for payload in [MARKER, "Yes, I accept", DIALOG] {
        let composer = format!("Claude Code\n❯ {payload}");
        let outputs = if payload.contains('\n') {
            vec![
                success("Claude Code"),
                success(""),
                success("Claude Code"),
                success(""),
                success(&composer),
                success(""),
            ]
        } else {
            vec![success("Claude Code"), success(""), success(&composer), success("")]
        };
        let (tmux, runner) = tmux(outputs);
        tmux.with_blocking_dialog_markers(vec![MARKER.into(), "Yes, I accept".into()])
            .send_keys("worker", payload)
            .await
            .unwrap();
        assert_eq!(runner.calls().last().unwrap().last().unwrap(), "Enter");
    }
    let transcript = "Claude Code\nThe test contains Do you want to proceed?\n1. Yes\n2. No\nEsc to cancel\n❯";
    let (tmux, _) = tmux([success(transcript), success("")]);
    tmux.with_blocking_dialog_markers(vec![MARKER.into()])
        .send_submit("worker")
        .await
        .unwrap();
}

#[tokio::test]
async fn blocking_dialog_rejects_every_input_shape_without_sending() {
    for path in ["literal", "paste", "return", "interrupt"] {
        let (tmux, runner) = tmux([success(DIALOG)]);
        let tmux = tmux.with_blocking_dialog_markers(vec![MARKER.into()]);
        let result = match path {
            "literal" => tmux.send_keys("worker", "3 approve globally").await,
            "paste" => tmux.send_keys("worker", "first\nsecond").await,
            "return" => tmux.send_submit("worker").await,
            "interrupt" => tmux.send_key("worker", "Escape").await,
            _ => unreachable!(),
        };
        let error = result.unwrap_err();
        assert_eq!(error.downcast_ref::<BlockingDialogOnScreen>().unwrap().marker, MARKER);
        assert!(error.to_string().contains("BlockingDialogOnScreen"));
        assert_eq!(runner.calls().len(), 1, "{path}: {:?}", runner.calls());
        assert_eq!(runner.calls()[0][2], "capture-pane");
        assert_eq!(&runner.calls()[0][2..], ["capture-pane", "-p", "-t", "worker"]);
    }
}

#[tokio::test]
async fn blocking_dialog_is_rechecked_between_text_and_return() {
    let (tmux, runner) = tmux([success("Claude Code"), success(""), success(DIALOG)]);
    let tmux = tmux.with_blocking_dialog_markers(vec![MARKER.into()]);
    let error = tmux.send_keys("worker", "status?").await.unwrap_err();
    assert!(error.is::<BlockingDialogOnScreen>());
    let calls = runner.calls();
    assert_eq!(calls.iter().filter(|args| args[2] == "send-keys").count(), 1);
    assert!(!calls.iter().any(|args| args.last().is_some_and(|arg| arg == "Enter")));
}

#[tokio::test]
async fn clear_screen_sends_and_capture_failure_refuses() {
    let (tmux, runner) = tmux([success("Claude Code"), success(""), success("Claude Code"), success("")]);
    tmux.with_blocking_dialog_markers(vec![MARKER.into()])
        .send_keys("worker", "hello")
        .await
        .unwrap();
    assert_eq!(runner.calls().last().unwrap().last().unwrap(), "Enter");

    let (tmux, runner) = super::tmux([failure("capture unavailable")]);
    assert!(
        tmux.with_blocking_dialog_markers(vec![MARKER.into()])
            .send_submit("worker")
            .await
            .is_err()
    );
    assert_eq!(runner.calls().len(), 1);
}

#[tokio::test]
async fn dialog_appearing_while_loading_a_buffer_prevents_paste_and_cleans_up() {
    let (tmux, runner) = tmux([success("Claude Code"), success(""), success(DIALOG), success("")]);
    let error = tmux
        .with_blocking_dialog_markers(vec![MARKER.into()])
        .send_keys("worker", "first\nsecond")
        .await
        .unwrap_err();
    assert!(error.is::<BlockingDialogOnScreen>());
    let calls = runner.calls();
    assert_eq!(
        calls.iter().map(|args| args[2].as_str()).collect::<Vec<_>>(),
        ["capture-pane", "load-buffer", "capture-pane", "delete-buffer"]
    );
    assert_eq!(calls[1][4], calls[3][4]);
}

#[test]
fn shared_remote_rejection_predicate_matches_host_display_envelope() {
    let error = BlockingDialogOnScreen { marker: MARKER.into() };
    assert!(BlockingDialogOnScreen::is_delivery_rejection(&format!(
        "deliver attempted: {error:#}"
    )));
    assert!(!BlockingDialogOnScreen::is_delivery_rejection(
        "deliver attempted: capture failed"
    ));
}
