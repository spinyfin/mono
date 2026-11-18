# PR #2890: Making markdown layout lazy by heading section

## 1. Problem being solved

Boss’s markdown viewer put ordinary documents into one `StructuredText` inside eager `VStack`s. Splitting was previously enabled only for explicitly collapsible headings. Consequently, a long headed document still presented its entire rendered block tree to layout.

Highlight updates added another source of work: changing `.id(parseVersion)` recreated the document subtree. Comment updates compensated by capturing the scroll offset and restoring it on the next run-loop turn. [Base rendering and refresh path](https://github.com/spinyfin/mono/blob/6f8ca35153d0fcae9aea5e9852894ea549989ca0/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L423-L547).

This guide covers the complete comparison from `6f8ca35153d0fcae9aea5e9852894ea549989ca0` to `12c538d6f328d9f4cbb62c0ef69d6fa9b301500a`. Code links are revision-pinned source links; exact diff-location URLs were unavailable.

## 2. Core fix / implementation

The fix makes **heading sections the unit of rendering and lazy layout**:

- Every recognized heading starts a chunk, whether or not any headings are collapsible.
- A direct-child `LazyVStack` exposes those chunks individually to the scroll view.
- Heading splits, document width selection, and comment-anchor projections are cached.
- Highlight changes update the markup passed to existing `StructuredText` views, instead of changing their SwiftUI identity.

Splitting also changes the scope in which comment anchors resolve. A new `MarkdownCommentAnchorMap` first selects a range across the concatenated chunk projections, then distributes that range to the overlapping chunks.

This is principally a layout optimization. Whole-document projection work still occurs, and a document without recognized headings remains one rendering chunk.

## 3. Implementation walkthrough

### Split the source into independently renderable sections

Start with [`MarkdownHeadingSections.chunks`](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L696-L720). The decisive change removes the early return for an empty collapsible-heading set and processes **all** detected headings:

```swift
let headings = headingLines(in: source)
guard !headings.isEmpty else { return [.plain(source)] }
```

Each section ends at the next heading of **any level**. Ordinary chunks retain their heading line; collapsible chunks separate the heading label from the body passed to the parser. Any text before the first heading becomes its own plain chunk.

The existing scanner recognizes nonempty ATX headings (`#` through `######`, with required following whitespace), allows up to three leading spaces, and skips content using its existing fence-toggle logic. This is a specific source scanner, not a complete Markdown block parser. [Heading recognition](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L729-L776).

### Expose those sections directly to lazy layout

The important structural change is in [`MarkdownDocumentColumn.body`](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L327-L377).

Condensed structure, with styling and measurement details omitted:

```swift
ScrollView {
    LazyVStack(alignment: .leading, spacing: 0) {
        header
        Divider()
        if let loadError {
            loadErrorView(loadError)
        }
        ForEach(loadError == nil ? renderedChunks : []) { entry in
            chunkView(entry)
                .bossMarkdown()
        }
    }
}
```

Both placements matter: the lazy stack is the scroll view’s document content, and `ForEach` exposes individual chunks directly within it. Applying `.bossMarkdown()` per chunk avoids wrapping the entire collection in one `BossMarkdownTheme` view. The [unchanged theme implementation](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/BossMarkdownStyle.swift#L870-L892) confirms that this modifier introduces a wrapper.

Spacing now belongs to explicit elements: header and divider each receive 12 points below them; later plain chunks receive top padding through `MarkdownChunkTopSpacing`. In editorial mode, that modifier uses the H2 top-spacing metric for every such chunk, rather than deriving spacing from each heading’s level. Collapsible sections retain their own padding. [Chunk spacing](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L803-L828).

### Worked example: the 24-section layout fixture

Use the actual [`largeHeadedDocument(sections: 24)` fixture](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Tests/BossTests/MarkdownDocumentChromeTests.swift#L669-L688). It contains a title and introductory paragraph, followed by 24 H2 sections, each with prose, nested list items, and a Swift code block.

Relevant state in the lazy-layout test:

```swift
let source = Self.largeHeadedDocument(sections: 24)
// MarkdownDocumentChrome defaults to no collapsible headings.
commentsEnabled: false
// Hosting viewport:
NSRect(x: 0, y: 0, width: 760, height: 400)
```

1. **Before:** the empty collapsible-heading set immediately returned `[.plain(source)]`. One `StructuredText` therefore owned the title and all 24 sections, inside the eager document stack. There was no section boundary at which the outer layout could defer rendering. [Base splitter](https://github.com/spinyfin/mono/blob/6f8ca35153d0fcae9aea5e9852894ea549989ca0/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L627-L653).
2. **After:** the splitter produces 25 plain chunks: title plus intro at index 0, then sections 1–24. Each becomes a separately identified child of the lazy stack. No extra prefix chunk is needed because the source starts with a heading.
3. **Observable result checked by the test:** after hosting and waiting for an appearance event, the probe must report more than zero but no more than `25 / 2`, or **12**, distinct chunk indices. This guards against broad initial appearance; it does not require an exact viewport-only count or establish a timing improvement. [Hosting test and assertions](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Tests/BossTests/MarkdownDocumentChromeTests.swift#L636-L666).

The useful boundary is heading-free prose: it still returns one `.plain(source)` chunk, so this change does not subdivide an arbitrarily large heading-free document.

### Reuse preprocessing and refresh highlights without remounting

The column stores the heading split and width selection in state. Appearance, source changes, and collapsible-heading configuration changes refresh them; ordinary body evaluations reuse the split when its source and heading keys match. There is a fallback split before the cache is current, so this is steady-state reuse, not a guarantee of exactly one scan per source. [Cache lookup and refresh](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L548-L585).

Highlight refresh uses a separate mechanism. The pinned [Textual implementation](https://github.com/gonzalezreal/textual/blob/5b06b811c0f5313b6b84bbef98c635a630638c38/Sources/Textual/StructuredText/StructuredText.swift) watches `markup` with `onChange(..., initial: true)`; swapping only the parser does not trigger parsing. `DocumentStructuredText` therefore passes:

```swift
StructuredText(
    MarkdownHighlightRefreshParser.tagged(text, generation: highlightGeneration),
    parser: MarkdownHighlightRefreshParser(inner: parser)
)
```

A nonzero generation appends `\n\n<!--boss-md-refresh:N-->`. The wrapper strips that suffix before invoking the real parser, preserving the source seen by comment and search highlighting. [Wrapper and nonce implementation](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L783-L852).

Comment anchors, flash anchors, and find navigation increment the generation. This removes the explicit `.id()` teardown and its offset-restoration workaround; it still reparses mounted text when highlights change. Find navigation continues to expand a matching collapsed section and request an approximate scroll position. [Refresh handlers](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L385-L414).

### Preserve comment semantics across rendering boundaries

Passing every document anchor to every new chunk would highlight repeated text independently in multiple sections. A quote spanning sections could also disappear because no individual chunk contains it.

[`MarkdownCommentAnchorMap`](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/Comments/MarkdownCommentAnchorMap.swift#L93-L161) addresses both cases:

1. Project each chunk’s `renderedText` to plain text and concatenate those projections without an added separator.
2. Resolve each anchor once against that concatenation.
3. Intersect the resulting character range with each chunk.
4. Construct local anchors containing the overlapping text and up to 64 characters of local context on either side.

The decisive overlap calculation is:

```swift
let overlapStart = max(match.lowerBound, chunkStart)
let overlapEnd = min(match.upperBound, chunkEnd)
guard overlapStart < overlapEnd else { continue }
```

Resolution remains best-effort: the [existing resolver](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/Comments/CommentHighlightingParser.swift#L148-L193) chooses the whitespace-tolerant occurrence with the best context score, preferring the earliest tie. Local parsers then resolve the clipped anchors again. Whitespace-only fragments are explicitly rejected; this is not a new general guarantee of unambiguous anchoring.

The map’s [reference-type cache](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/Comments/MarkdownCommentAnchorMap.swift#L28-L71) separates projection validity from highlight state. Source, heading configuration, base URL, and chunk count determine projection reuse. Changing only comments or flash state recomputes assignments without rebuilding projections.

Two related limits matter:

- Projection creation remains eager for all chunks, even with no anchors. Find state also maintains its own projected corpora.
- Find searches **within each chunk**, unlike comment resolution over concatenated projections. It does not discover a query spanning chunk boundaries. [Unchanged find computation](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/Search/MarkdownFindState.swift#L169-L189).

Finally, timing now combines per-chunk heights using `max`. A positive height can trigger the existing log without waiting for every off-screen section, so `phase=parse` is no longer evidence of complete document layout. [Height preference](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L231-L237).

## 4. Tests and supporting infrastructure

**Added tests:**

- **Heading-free fallback:** asserts that ordinary paragraphs remain exactly one plain chunk. [Test](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Tests/BossTests/MarkdownDocumentChromeTests.swift#L317-L322).
- **Highlight nonce:** checks unchanged untagged input, changed tagged input, exact stripping, and generation-zero behavior. It exercises string helpers, not a mounted highlight update. [Test](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Tests/BossTests/MarkdownDocumentChromeTests.swift#L503-L510).
- **Comment partitioning:** repeated `checkleft run` text selects only the Alpha chunk using context; a quote built across a heading boundary assigns highlights to both chunks. Both tests run the local highlighting parsers and count chunks containing background attributes. The whitespace test only calls `isAnchorable` with `"\n"`; it does not exercise a full clipping operation. [Anchor tests](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Tests/BossTests/MarkdownDocumentChromeTests.swift#L517-L610).
- **Large-document hosting and laziness:** asserts 25 chunks and positive fitting height, then separately checks the bounded appearance count described above. [Layout tests](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Tests/BossTests/MarkdownDocumentChromeTests.swift#L617-L666).

**Modified/replaced tests:** the former empty-set and nonmatching-set “single plain chunk” tests now require splitting on real headings; the empty-set test also checks exact source reconstruction. The fenced-code test now expects two real-heading chunks while rejecting the fenced fake heading. Those old single-chunk expectations are replaced, not retained. Existing folding and find-index mapping tests remain. [Updated splitting assertions](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Tests/BossTests/MarkdownDocumentChromeTests.swift#L289-L322), [fence assertions](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Tests/BossTests/MarkdownDocumentChromeTests.swift#L390-L413).

**Infrastructure:** tests add an AppKit import, a generated large-document fixture, a background-attribute helper, and an environment-injected appearance probe. The lazy test hosts an `NSWindow` and pumps the run loop until the first appearance or a two-second deadline. The probe records `onAppear`, not constructor calls or layout passes.

No build/configuration files change. Existing [source globs](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/BUILD.bazel#L58-L64) include the new map, and the existing [Shard3 glob](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/BUILD.bazel#L278-L284) includes the test file.

The [PR description](https://github.com/spinyfin/mono/pull/2890) reports the app-library/Shard3 build, `bazel test //tools/boss/app-macos:BossTestsShard3`, and `checkleft run`. I inspected the comparison and relevant source; I did **not** run those checks. The added tests do not directly verify cache reuse, scroll preservation during highlight updates, or a measured reduction in layout time.
