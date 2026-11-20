# PR #2890: Render Markdown documents section by section

## Orientation

Readers should be able to open and interact with long Markdown documents without repeatedly laying out the whole document. This performance fix changes the shared document viewer in the Boss macOS app so it can defer rendering sections outside the visible area and update comment or search highlights without tearing down the document view.

The viewer divides a document at headings into **chunks**, meaning independently rendered sections. A **lazy stack** creates section views as they are needed for display. Previously, ordinary documents were rendered as one large section; the new structure also reuses the heading split between routine view updates. Comment highlights continue to identify their selected text even when that text repeats in several sections or crosses a section boundary.

## Change overview

The change combines three related improvements:

- Ordinary headed documents now receive the same section-based rendering structure previously reserved for explicitly collapsible content.
- The scrolling view exposes those sections directly to lazy layout and retains source-derived data across view updates.
- Highlight changes reparse existing section views instead of replacing the entire document subtree.

The scope is the shared `MarkdownDocumentChrome` viewer, already used by the [disk-backed document window](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/DesignRendererView.swift#L153-L180), the [embedded Designs reader](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/DesignsView.swift#L444-L457), and the [string-backed document wrapper](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/DesignsView.swift#L582-L598). Those callers inherit the behavior through the shared component.

## Core implementation

`StructuredText` is the Markdown-rendering view supplied by the Textual library. The implementation now supplies one such view per heading section, inside `LazyVStack`, the SwiftUI container that defers creating off-screen children.

The data path is: source text → cached heading chunks → per-chunk comment and search parsers → independently rendered section views. A parser converts Markdown into styled text. The new `MarkdownCommentAnchorMap` resolves document-wide comment selections before distributing them to those parsers; `DocumentStructuredText` handles highlight refreshes while retaining view identity. The integration is concentrated in [`renderedChunks` and `markdownParser`](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L546-L610).

## Implementation walkthrough

### 1. Split ordinary documents, then keep the split

The [old splitter](https://github.com/spinyfin/mono/blob/6f8ca35153d0fcae9aea5e9852894ea549989ca0/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L627-L648) immediately returned one plain chunk when no collapsible headings were configured. The [new splitter](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L697-L720) visits every recognized heading:

```swift
let headings = headingLines(in: source)
guard !headings.isEmpty else { return [.plain(source)] }
```

Each section ends at the next heading of any level. Ordinary sections retain their heading line; explicitly collapsible sections separate the heading label from the body. Text before the first heading becomes its own plain chunk. The existing [heading scanner](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L735-L775) recognizes headings introduced by one to six hash marks and skips fenced code.

The [chunk cache](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L571-L585) returns the stored array when source and collapsible-heading settings match. Refreshing it also stores the document width selected for prose or tables. Refreshes occur [on appearance and when source or heading settings change](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L385-L398); a cache miss can compute chunks before that refresh lands.

### 2. Make the section boundaries visible to lazy layout

The decisive architectural change is the [direct relationship between the scroll view, lazy stack, and section loop](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L327-L379). Condensed pseudocode:

```swift
ScrollView {
    LazyVStack(alignment: .leading, spacing: 0) {
        header
        divider
        optionalLoadError
        ForEach(loadError == nil ? renderedChunks : []) { entry in
            chunkView(entry).bossMarkdown()
        }
    }
}
```

The theme applies to each section rather than wrapping the entire loop. The error branch supplies an empty section array rather than enclosing the loop in another conditional container. Together, these choices leave the individual sections available to lazy layout.

Spacing also moves to explicit locations: the header and divider receive 12-point bottom padding, and [later plain chunks receive top padding](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L802-L825). Collapsible sections retain their own spacing.

Height reporting now comes from each chunk. The [height preference takes the maximum](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L227-L235), and the existing [timing handler](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L412-L425) logs after a positive reported height when timing is enabled. This is a layout signal from rendered chunks, rather than a sum of the complete document height.

**Worked example: a long document in a short window.** The [test fixture](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Tests/BossTests/MarkdownDocumentChromeTests.swift#L669-L688) contains a title and 24 sections, each with prose, a nested list, and fenced Swift code. Comments are disabled and no collapsible headings are configured.

1. **Before:** the empty collapsible-heading set produced one chunk for the entire source. The [ordinary stack rendered that chunk as one `StructuredText`](https://github.com/spinyfin/mono/blob/6f8ca35153d0fcae9aea5e9852894ea549989ca0/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L439-L467), so there was no section boundary at which layout could defer off-screen content.
2. **After:** the splitter produces 25 chunks: the title section and 24 numbered sections. Each becomes a separate child of the lazy stack.
3. **Observable result:** in the [760 × 400 hosting test](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Tests/BossTests/MarkdownDocumentChromeTests.swift#L636-L667), the assertion requires at least one and at most 12 distinct chunk appearances. The initial display can therefore leave much of the document uninstantiated. Routine updates reuse the heading split.

A document without recognized headings still takes the single-chunk path.

### 3. Preserve comment meaning across independent sections

A **comment anchor** stores selected text plus surrounding text to distinguish repeated occurrences. A **plain-text projection** is the rendered text with Markdown formatting removed; the existing [projection helper](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/Comments/CommentWire.swift#L305-L321) obtains it from the Markdown parser.

Passing every anchor to every section would let the same phrase highlight once in each section. The new map instead [joins the section projections](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/Comments/MarkdownCommentAnchorMap.swift#L92-L102), resolves each anchor once, and distributes only its overlaps:

```swift
let overlapStart = max(match.lowerBound, chunkStart)
let overlapEnd = min(match.upperBound, chunkEnd)
guard overlapStart < overlapEnd else { continue }
let local = (overlapStart - chunkStart)..<(overlapEnd - chunkStart)
let clipped = clip(plains[index], to: local)
guard isAnchorable(clipped) else { continue }
```

The [assignment and clipping code](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/Comments/MarkdownCommentAnchorMap.swift#L124-L166) constructs local anchors with up to 64 surrounding characters on each side. A selection crossing a heading can reach both chunks; whitespace-only fragments are discarded. An unresolved anchor produces no assignment.

The existing [resolver](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/Comments/CommentHighlightingParser.swift#L146-L184) chooses the occurrence whose surrounding text scores best, preferring the earlier occurrence on a tie. For example, the [repeated-phrase test](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Tests/BossTests/MarkdownDocumentChromeTests.swift#L517-L548) puts “checkleft run” under both Alpha and Beta. The suffix “ here” selects Alpha; only that chunk receives and paints the highlight.

The [map cache](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/Comments/MarkdownCommentAnchorMap.swift#L40-L70) separates projection reuse from anchor assignment. Projection validity depends on source, heading settings, base URL, and chunk count. Changing comments or the flashing selection reassigns anchors using that projection; unchanged inputs return the previous assignments.

### 4. Refresh highlights without replacing the document

Previously, [`.id(parseVersion)` replaced the document subtree](https://github.com/spinyfin/mono/blob/6f8ca35153d0fcae9aea5e9852894ea549989ca0/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L463-L469). Comment updates also [captured and asynchronously restored the scroll offset](https://github.com/spinyfin/mono/blob/6f8ca35153d0fcae9aea5e9852894ea549989ca0/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L530-L544).

Now a **highlight generation** is a counter incremented when comments, comment flashing, or find navigation change. [`DocumentStructuredText`](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L784-L799) passes that counter through the markup string:

```swift
StructuredText(
    MarkdownHighlightRefreshParser.tagged(text, generation: highlightGeneration),
    parser: MarkdownHighlightRefreshParser(inner: parser)
)
```

The [adapter](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L832-L853) appends a trailing refresh tag for nonzero generations, then strips it before invoking the real parser. This changes the markup input to trigger reparsing while keeping the section view identity and original text offsets. The former scroll snapshot/restore methods are removed.

Find retains its existing [per-chunk match computation](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/Search/MarkdownFindState.swift#L112-L129) and now inherits the broader heading split. The viewer still [expands a collapsed section containing the current match and scrolls toward it](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L399-L410); that navigation uses an approximate text-position fraction.

## Tests and test infrastructure

**Modified tests:** the two former single-chunk expectations now require heading-based splitting even with an empty or nonmatching collapsible set. They check chunk shape and, for the empty set, exact source reconstruction. The fenced-code test now expects two real heading sections while rejecting the heading-like line inside the fence. See [split expectations](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Tests/BossTests/MarkdownDocumentChromeTests.swift#L289-L314) and [fence handling](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Tests/BossTests/MarkdownDocumentChromeTests.swift#L387-L413). Those old expectations are replaced; no additional tests are removed.

**Added tests:**

- [No headings](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Tests/BossTests/MarkdownDocumentChromeTests.swift#L316-L321): preserves one unchanged plain chunk.
- [Refresh tag](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Tests/BossTests/MarkdownDocumentChromeTests.swift#L503-L510): generation seven changes the input, stripping restores it, and generation zero leaves it unchanged.
- [Comment partitioning](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Tests/BossTests/MarkdownDocumentChromeTests.swift#L517-L607): repeated text assigns and paints one chunk; a boundary-spanning selection assigns and paints two; a newline-only anchor fails the anchorability predicate.
- [Large-document hosting and lazy appearance](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Tests/BossTests/MarkdownDocumentChromeTests.swift#L617-L667): verifies 25 chunks and positive hosted height, then checks the bounded appearance count in a short window.

Infrastructure additions include the structured large-document fixture, a helper detecting background-color highlights, and an [optional environment-injected appearance probe](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L237-L255). The window test waits for the first appearance for up to two seconds, then rejects broad instantiation rather than requiring an exact visible count.

Build configuration is unchanged: existing [source globbing](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/BUILD.bazel#L58-L64) includes the new map, and the existing [Shard 3 test library](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/BUILD.bazel#L278-L291) includes the updated test file.

## Evidence

The PR description reports the following validation:

- The author ran `bazel build //tools/boss/app-macos:boss_mac_app_lib //tools/boss/app-macos:BossTestsShard3`.
- The author ran `bazel test //tools/boss/app-macos:BossTestsShard3`.
- The author ran `checkleft run`.

No screenshots are registered.
