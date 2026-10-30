/// Review-brief markdown used by revision-brief regression tests and by
/// quiet viewer capture (`--capture-brief-markdown` pointing at a file of
/// these bytes).
///
/// `engineShaped` snapshots the layout `render_revision_instructions`
/// emits in `tools/boss/engine/pr-review/src/render.rs`: a blank line
/// before the first finding, two findings each followed by a blank line,
/// and a trailing `**Review summary:**`. `adjacentFindingHeading` is the
/// no-blank-line neighbour of that shape, kept so the wrapped-paragraph
/// split cannot regress on either boundary.
enum RevisionBriefCaptureFixture {
    /// Engine output: blank line after the boilerplate, two findings, summary tail.
    static let engineShaped = """
    Automated PR review of PR #3304 found 2 finding(s) requiring attention.
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

    Verified: dispatch only ensures the result fits the local write buffer.

    ### [medium] Quiet capture should pin the engine blank-line boundary
    **File:** `tools/boss/app-macos/Tests/BossTests/RevisionBriefCaptureFixture.swift`  
    **Category:** tests  
    **Confidence:** medium

    Second finding keeps the multi-finding sequence and summary tail in the fixture.

    **Review summary:** Two findings require a fix.
    """

    /// Same boilerplate with the first finding heading on the next line.
    static let adjacentFindingHeading = """
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
