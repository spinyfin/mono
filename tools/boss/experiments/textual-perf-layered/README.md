# textual-perf-layered

Layered bisection rig for Boss markdown render slowness (mono#688). Builds on the Textual-only baseline from `tools/boss/experiments/textual-perf/` (PR #686) by re-introducing Boss's wrappers one at a time, so the layer that crosses from milliseconds into seconds is the offender.

See `tools/boss/docs/investigations/markdown-render-slowness-2026-05-18.md` for the static-analysis writeup and hypothesis ranking the rig is designed to validate.

## Run

```sh
cd tools/boss/experiments/textual-perf-layered
swift run textualperflayered
```

In another terminal, stream the timing logs:

```sh
log stream --predicate 'subsystem == "com.boss.textualperf"' --level info
```

Use the segmented picker at the top to switch between layers. Each picker click logs `phase=parse_start layer=Ln`, and the first non-zero `StructuredText` height fires `phase=parse_end layer=Ln duration_ms=…`. The pane is keyed by `.id(layer)`, so re-clicking a layer captures a fresh sample.

### Headless / automated runs

Set `TPL_AUTO=1` to cycle every layer automatically without clicking the picker — the driver steps through the layers, takes `TPL_ITERS` samples each (waiting for each `parse_end` before advancing), logs a `phase=auto_layer layer=Ln samples_ms=[…]` summary per layer, then logs `phase=auto_done` and exits. Useful when a human can't sit and click, and for reproducible numbers.

Environment knobs:

| Var | Default | Meaning |
|-----|---------|---------|
| `TPL_AUTO`    | unset | `1` enables the auto-driver; otherwise the picker is manual |
| `TPL_ITERS`   | 3     | samples per layer in auto mode |
| `TPL_TIMEOUT` | 90    | per-sample timeout (s) before logging `parse_timeout` and advancing |
| `TPL_LAYERS`  | all   | comma-separated subset to run, e.g. `L6,L7,L8,L9` |
| `TPL_PUB_MS`  | 500   | sibling-publisher (L7+) / L10-host publish interval (ms) |
| `BOSS_SAMPLE_MD` | bundled 47 KB doc | absolute path to the markdown sample to render |

> **Render-complete signal.** The height the `parse_end` logic keys on is reported via a *downward-flowing* `reportRenderHeight` environment closure, not an upward-bubbling `PreferenceKey`. The earlier `PreferenceKey` approach silently failed for the wrapped layers (L3+) — `withCommentsStub`'s `HStack`/`.overlay`/`.background` disrupted preference propagation, so those layers never recorded a `parse_end` and the "L1–L5 fine" reading was eyeball-only. The environment closure reaches the reporter regardless of intervening wrappers.

`bazel run //tools/boss/experiments/textual-perf-layered:textualperflayered` also builds the .app, but the working directory is whatever Bazel sets and the relative path resolution won't find the sample — set `BOSS_SAMPLE_MD=/absolute/path/to/sample.md` when running under Bazel.

## Sample source

Defaults to `tools/boss/docs/designs/installable-distribution-package-for-boss.md` (the same 47 KB doc the textual-perf rig in PR #686 uses). Resolution order:

1. `BOSS_SAMPLE_MD` env var (absolute path).
2. Walks up from the current working directory looking for `tools/boss/docs/designs/installable-distribution-package-for-boss.md`.
3. Falls back to a 1 KB placeholder with an error banner if neither resolves.

The doc is *not* duplicated into this experiment's Resources folder — keeping the diff small and the rig pointed at the live design-doc source.

## Layers

| Layer | Adds                                                  | Hypothesis it isolates                                                                 |
|-------|-------------------------------------------------------|----------------------------------------------------------------------------------------|
| L0    | nothing — matches PR #686                             | baseline (~190 ms)                                                                     |
| L1    | `.bossMarkdown()`                                     | Boss's table-Canvas overlay / code-block / blockquote / heading styles                 |
| L2    | Boss inner wrappers (frame, dual text-selection, title, double padding) | nested `frame(maxWidth: .infinity)` + two text-selection modifiers           |
| L3    | `.withComments()` stub                                | HStack-wrap, `@StateObject` rebuild surface, environment injection                     |
| L4    | view-model `.loading` → `.loaded` flip                | view-model rebuild on `renderContentID` UUID change                                    |
| L5    | view-model + async fetch                              | spinner → content transition cost                                                      |
| L6    | passive `ChatViewModelStub` as `@EnvironmentObject`   | cost of EnvironmentObject subscription graph without any active publishing             |
| L7    | L6 + `SiblingPublisherStub` firing every ~500 ms      | sibling-publisher invalidation cascade forcing design-doc body re-evaluation           |
| L8    | L7 + local NSEvent monitors (keyDown, rightMouseDown, leftMouseUp) | event-monitor overhead on main-thread availability during render               |
| L9    | L8 + `ExtraViewModelStub` at ~350 ms cadence          | combined publish load from all active observers (full production scene complement)     |
| L10   | mount-latency probe (not a parse layer)               | *the actual root cause* — see below                                                    |

### L10: mount-latency / observability probe

L0–L9 all measure parse+layout time. **Production's `phase=render` does not** — it measures the latency from `state = .loaded` to the loaded view *mounting* (`AsyncMarkdownViewerView`'s `.loaded`-branch `.onAppear`), which excludes the `StructuredText` parse. L10 targets that window. It contrasts two observation patterns and logs `phase=obs pattern=buggy|fixed latency_ms=… pub_ms=…`:

- **buggy** (what production shipped): a view observes a *host* `ObservableObject` but reads a *nested* object's state through it, without observing the nested object. The nested state change is only picked up on the next host publish, so mount latency tracks `TPL_PUB_MS`.
- **fixed**: the view observes the nested object directly → mount latency is ~constant few ms, independent of host publish timing.

Measured (buggy tracks the publish interval; fixed is ~3 ms regardless):

| `TPL_PUB_MS` | buggy | fixed |
|--------------|-------|-------|
| 500          | ~210 ms  | ~3 ms |
| 3000         | ~2200 ms | ~3 ms |

This reproduces `AsyncMarkdownViewerView` reading `chatModel.asyncMarkdownViewerVM.state` without observing the (nested, non-`@Published`) `asyncMarkdownViewerVM`. Under main-thread contention the gap to the next `chatModel` publish stretches to the tens of seconds seen as the wall. The production fix observes the VM directly. See `tools/boss/docs/investigations/markdown-render-slowness-2026-05-18.md`.

The comments stub (L3+) is intentionally a `@Published`-surface lookalike without NSEvent monitors. Adding global event monitors from a benchmark rig is hazardous (they leak across runs and intercept other apps' shortcuts), and the monitors don't fire during render — they only fire on user key/right-click events. If the rig's L3 shows the slowness, the cause is in the wrapper structure, not the monitors.

### L6–L9: what each layer simulates

**L6 — passive EnvironmentObject**: In production, `BossMacApp` creates `@StateObject private var chatModel = ChatViewModel(...)` and injects it via `.environmentObject(chatModel)` on the scene. The async-markdown-viewer `Window` scene receives `chatModel` as an `@EnvironmentObject`. `ChatViewModelStub` has ~20 `@Published` properties (matching the structural shape of `ChatViewModel`) but never publishes — so L6 = L5 + "is there a large EnvironmentObject in the tree at all?"

**L7 — active sibling publisher**: `SiblingPublisherStub.start()` fires a Task that increments `tickCount` every ~500 ms. This approximates the combined publish cadence of ChatViewModel (engine events), the kanban view-models (task/project updates), and the live-status pollers observed during earlier sessions (kanban resolve spiked from ~170 ms → 1,427 ms alongside the 38 s render). L7 = L6 + "do sibling objectWillChange events cascade into this view?"

**L8 — NSEvent monitors**: Installs the three local monitors that `CommentLayer.installMonitors()` registers on appear: `.keyDown`, `.rightMouseDown`, `.leftMouseUp`. All handlers pass events through unchanged. Production's monitors run the `captureInteractionAnchor` / `shouldConsumeKeyEvent` paths on every event; the rig's are no-ops. L8 = L7 + "does the event-loop interception cost affect main-thread availability?"

**L9 — full scaffold**: Adds `ExtraViewModelStub` (publishing every ~350 ms) on top of L8, mirroring `WorkersWorkspaceModel` and `BossPaneModel` from `ContentView`'s other `@StateObject` declarations. L9 = L8 + "does the total combined publish load across all active observers reproduce the wall?"

## Reading the output

`phase=parse_end layer=Ln duration_ms=<n>` is the headline number for each layer. Capture 3+ samples per layer (re-click the picker) and average — first render of any layer pays one-time SwiftUI-init costs, so the second and third runs are more representative.

The on-screen overlay in the bottom-right of each pane shows the same numbers in case you don't want to keep `log stream` open.

## Not measured

- **Async attachment resolution.** `WithAttachments` resolves image URLs asynchronously *after* `parse_end` fires, so it doesn't affect the headline number. Boss's design docs typically don't have images, so this is fine.
- **Code tokenization for highlighting.** `HighlightedTextFragment.tokenize(...)` runs asynchronously and updates the highlighted code via state change *after* the initial render. The 7 code blocks in the 47 KB doc each get one async tokenize task, but those don't move `parse_end`.
- **HighlightingMarkdownParser path.** With zero comments, Boss uses `AttributedStringMarkdownParser.markdown(...)` — same parser as L0. The highlighting wrapper only matters if comments exist; this rig measures the no-comments case to stay comparable with PR #686.
