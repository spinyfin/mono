use super::*;

const MARKER: &str = "Do you want to proceed?";

#[tokio::test]
async fn blocking_dialog_rejects_every_input_shape_without_sending() {
    for path in ["literal", "paste", "return", "interrupt"] {
        let (tmux, runner) = tmux([success(MARKER)]);
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
    let (tmux, runner) = tmux([success("Claude Code"), success(""), success(MARKER)]);
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
    let (tmux, runner) = tmux([success("Claude Code"), success(""), success(MARKER), success("")]);
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
