/// Exact review brief used by the quiet viewer capture and regression tests.
enum RevisionBriefCaptureFixture {
    static let markdown = """
    Automated PR review of PR #3304 found 10 finding(s) requiring attention.
    Address ALL findings before finalising this revision.

    ## HARD RULE: no punting — do the actual work

    Each finding below requires a real code change that resolves it.
    The following are FORBIDDEN — they do NOT count as addressing a finding:

    - Filing a follow-up task or issue in lieu of fixing the finding.
    - Leaving a `TODO`, `FIXME`, `Phase N`, "wire this later", or similar
    deferral comment and calling the finding done.
    - Acknowledging the finding in a comment or PR description without making
    the substantive code change it requires.
    - Marking the revision complete while a finding's core work is unaddressed.

    For example: a "write-only field" finding requires wiring the READ/consumer
    side so the field is actually used — not a TODO comment promising to do so
    later, and not a follow-up task filed for someone else.

    **If — and only if — a finding genuinely cannot or should not be fixed within
    this revision's scope**, you must EXPLICITLY SURFACE that in your response:
    state which finding you are not fixing and exactly why (technical blocker,
    out-of-scope dependency, requires a decision from the repo owner, etc.). Do NOT silently
    file a follow-up or leave a TODO. The default is to do the work.
    ### [high] Workspace result admission ignores negotiated frame/credit limits; unsendable result wedges the daemon
    **File:** `tools/boss/engine/host-execution/src/lib.rs` (workspace::Executor::dispatch result-size ensure ~L1762-1766)  
    **Category:** correctness  
    **Confidence:** medium

    Verified: dispatch only ensures ...
    """
}
