# PR #2890: Make markdown layout lazy without losing highlights

This guide covers base `6f8ca35153d0fcae9aea5e9852894ea549989ca0` → head `12c538d6f328d9f4cbb62c0ef69d6fa9b301500a`. Code links are revision-pinned.

## 1. Problem being solved

Boss’s markdown viewer put ordinary documents through one `StructuredText` inside eager `VStack`s. Heading-based splitting existed only for explicitly collapsible sections. Consequently, a long headed document still presented its entire rendered tree for layout, including content far outside the viewport.

Highlight updates compounded the cost: changing comments or find state changed `.id(parseVersion)`, recreating the document body. Comment updates attempted to preserve the viewport by capturing and asynchronously restoring its offset. See the [base rendering and remount path](https://github.com/spinyfin/mono/blob/6f8ca35153d0fcae9aea5e9852894ea549989ca0/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L423-L547).

The change targets repeated whole-document **layout and reconstruction**. It does not eliminate every whole-document scan or parse.

## 2. Core fix / implementation

The implementation combines three changes:

- **Render heading sections independently**, making them direct children of the scroll view’s `LazyVStack`.
- **Cache source-derived work**, including the heading split, document width choice, and comment-anchor text projection.
- **Refresh highlights without changing view identity**, by changing the markup string passed to `StructuredText` and stripping the refresh marker before parsing.

Splitting also changes the scope in which comment anchors resolve. A new `MarkdownCommentAnchorMap` resolves each anchor against the concatenated chunk projections, then assigns clipped anchors to overlapping chunks. This preserves document-wide selection behavior while rendering sections separately.

The central integration point is [`MarkdownDocumentColumn`](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L294-L410).

## 3. Walkthrough of the logic and architecture

### Split first, then expose the chunks to lazy layout

[`MarkdownHeadingSections.chunks`](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L697-L720) now processes every recognized heading, even when `collapsibleHeadings` is empty:

```swift
let headings = headingLines(in: source)
guard !headings.isEmpty else { return [.plain(source)] }
```

Each section ends at the next heading of **any level**. Ordinary chunks retain their heading line; opted-in collapsible chunks separate the heading label from the body. Content preceding the first heading becomes its own plain chunk.

The unchanged scanner recognizes ATX headings and skips fenced code using the existing fence toggle. This is not a complete Markdown structural parser; for example, setext headings are not splitting boundaries. See the [heading scanner](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L730-L775).

The corresponding view hierarchy is equally important. **Condensed structure:**

```swift
ScrollView {
    LazyVStack(alignment: .leading, spacing: 0) {
        header
        Divider()
        // Optional error row
        ForEach(loadError == nil ? renderedChunks : []) { entry in
            chunkView(entry)
                .bossMarkdown()
                // Per-chunk environments and height reporting
        }
    }
}
```

The `ForEach` sits directly inside the lazy stack, and `.bossMarkdown()` moves onto each chunk. That modifier actually creates a [`BossMarkdownTheme` wrapper view](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/BossMarkdownStyle.swift#L870-L892); wrapping the entire collection would hide the individual rows behind that wrapper. See the [new layout hierarchy](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L327-L379).

Spacing becomes explicit: header and divider each get 12-point bottom padding, while subsequent plain chunks receive heading top spacing. In editorial mode that uses the H2 spacing metric, irrespective of the particular heading’s level.

### Worked example: the 24-section test document

Use the actual [`largeHeadedDocument(sections: 24)` fixture](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Tests/BossTests/MarkdownDocumentChromeTests.swift#L669-L688): one `# Large document` heading followed by 24 `## Section …` headings, each with prose, a nested list, and fenced Swift code.

Relevant state is the default empty collapsible-heading set, comments disabled, and a 760 × 400 viewport.

| Step              | Base behavior                                                         | Head behavior                                                                                               |
| ----------------- | --------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------- |
| Split source      | The empty collapsible set immediately returns one `.plain(source)`.   | The scanner finds 25 headings and produces 25 plain chunks.                                                 |
| Build render tree | One `StructuredText` contains the entire document under eager stacks. | Each chunk has its own `DocumentStructuredText`, exposed through the direct-child `ForEach`.                |
| Initial layout    | There is no section-level lazy boundary within the document.          | The lazy stack can defer off-screen section views.                                                          |
| Observable check  | No chunk-appearance probe exists.                                     | The hosting test requires at least one, but at most `25 / 2 = 12`, distinct chunk indices to have appeared. |

The decisive before/after is the [old empty-set shortcut](https://github.com/spinyfin/mono/blob/6f8ca35153d0fcae9aea5e9852894ea549989ca0/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L627-L650) becoming [all-heading splitting](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L697-L720), combined with the lazy hierarchy above. The [hosting assertion](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Tests/BossTests/MarkdownDocumentChromeTests.swift#L636-L666) specifies the expected observation; it is not a measured result from this review or a guarantee of an exact visible-row count.

A useful boundary: a document without recognized headings remains one chunk, so this change provides no section-level layout subdivision for it.

### Keep ordinary invalidations from repeating source-derived work

[`documentChunks` and `refreshCachedChunks`](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L571-L585) cache the split by source and collapsible-heading configuration. Refresh runs on appearance and changes to either input; a cache mismatch can still compute a fallback split during body evaluation. The same refresh stores the table-dependent document width.

The [comment cache](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/Comments/MarkdownCommentAnchorMap.swift#L40-L70) separates two invalidation levels:

- Source, heading set, base URL, or chunk-count changes rebuild the plain-text projection.
- Highlighted/flashing anchor changes reuse that projection and redo anchor assignment.

A completely unchanged input set returns the previous assignment. This relies on the caller deriving chunk contents from the supplied source and heading set; the cache does not compare every chunk’s contents independently.

**Work still done eagerly:** projection construction parses every chunk, even with no anchors; `MarkdownFindState.updateSource` separately builds its own chunk corpora. `renderedChunks` also rebuilds the array of parser values. The benefit is deferred rendering/layout and less repeated preprocessing, not wholly lazy document parsing. See [projection construction](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/Comments/MarkdownCommentAnchorMap.swift#L93-L112) and [existing find-state setup](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/Search/MarkdownFindState.swift#L111-L117).

### Preserve document-wide comment matching across section boundaries

The existing [`resolveRange`](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/Comments/CommentHighlightingParser.swift#L146-L184) returns one whitespace-tolerant match, using prefix/suffix scores when there are multiple candidates and preferring the earliest on ties. Supplying every chunk with the original anchor list would therefore select one occurrence **per chunk**, or miss an anchor spanning chunks.

The new map concatenates the per-chunk projections without adding separators, resolves globally, and intersects the resulting character range with each chunk:

```swift
let overlapStart = max(match.lowerBound, chunkStart)
let overlapEnd = min(match.upperBound, chunkEnd)
guard overlapStart < overlapEnd else { continue }
let local = (overlapStart - chunkStart)..<(overlapEnd - chunkStart)
let clipped = clip(plains[index], to: local)
guard isAnchorable(clipped) else { continue }
```

[`clip`](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/Comments/MarkdownCommentAnchorMap.swift#L125-L165) creates a local `exact` plus up to 64 characters of prefix and suffix. The chunk parser then resolves that local quote again. Unresolved anchors and whitespace-only fragments are dropped.

This preserves the ordinary document’s single selected occurrence after splitting; it is not evidence that the unsplit base duplicated highlights. Search remains different: its unchanged algorithm matches within each chunk, so it does not gain cross-boundary matching from the comment map.

### Refresh highlights in place

The pinned Textual implementation watches [changes to `markup`, not changes to the parser instance](https://github.com/gonzalezreal/textual/blob/5b06b811c0f5313b6b84bbef98c635a630638c38/Sources/Textual/StructuredText/StructuredText.swift). The PR therefore replaces identity changes with a generation-tagged input:

```swift
StructuredText(
    MarkdownHighlightRefreshParser.tagged(text, generation: highlightGeneration),
    parser: MarkdownHighlightRefreshParser(inner: parser)
)
```

Nonzero generations append `\n\n<!--boss-md-refresh:N-->`; the wrapper removes that suffix before invoking the real parser. Thus the observed markup changes while the parser receives the original source and character offsets. See [`DocumentStructuredText` and the refresh parser](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L784-L852).

Comment, flash, and find-navigation changes increment the generation. Chunk identity remains its document-order index, and the old offset capture/restore workaround disappears. Find navigation still intentionally requests scrolling and expands a matching collapsed section.

Finally, height reporting moves onto chunks and reduces with `max`. The timing callback now detects a positive chunk height; it does **not** establish that every off-screen section has parsed or laid out. See [height reduction](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L230-L235) and the [logging condition](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L412-L425).

## 4. Tests and supporting infrastructure

**Added tests:**

- [No-heading fallback](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Tests/BossTests/MarkdownDocumentChromeTests.swift#L315-L321): preserves one unchanged plain chunk.
- [Refresh tagging](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Tests/BossTests/MarkdownDocumentChromeTests.swift#L503-L509): generation zero leaves source unchanged; generation seven changes it; stripping recovers the source.
- [Recurring comment quote](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Tests/BossTests/MarkdownDocumentChromeTests.swift#L517-L548): context selects Alpha rather than Beta, and parsing the assigned anchors produces a background highlight in exactly one chunk.
- [Cross-heading anchor](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Tests/BossTests/MarkdownDocumentChromeTests.swift#L554-L606): a quote constructed across the actual projection boundary highlights both chunks. A separate test rejects a newline-only anchor through `isAnchorable`; it does not exercise the full clipping path.
- [Large-document hosting and laziness](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Tests/BossTests/MarkdownDocumentChromeTests.swift#L617-L688): asserts 25 chunks and positive hosting height, then checks the bounded appearance count described above.

**Modified tests:** the empty and nonmatching collapsible-set tests now expect heading splitting rather than one chunk. The fenced-code test expects two real heading chunks, excludes the fenced pseudo-heading, and checks exact source reconstruction. See [split expectations](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Tests/BossTests/MarkdownDocumentChromeTests.swift#L289-L321) and [fence coverage](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Tests/BossTests/MarkdownDocumentChromeTests.swift#L391-L413). No standalone test scenario is removed; the two obsolete single-chunk expectations are replaced.

**Infrastructure:** the test file adds AppKit, a generated large-document fixture, and a helper detecting attributed-string background colors. A new environment-injected [`MarkdownChunkAppearProbe`](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L237-L258) records distinct chunk appearances. The window-backed test pumps the run loop until an appearance occurs or two seconds elapse; it does not observe a full two-second settling period.

No build configuration changes. Existing [source globbing](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/BUILD.bazel#L58-L64) includes the new map, and the existing [Shard3 glob](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/BUILD.bazel#L278-L291) includes these tests.

The coverage establishes structural laziness and selected highlighting behavior. It does not directly assert elapsed layout cost, cache-hit counts, or viewport preservation through an actual highlight update.

The supplied PR description reports building the app library and `BossTestsShard3`, running that test shard, and running `checkleft run`. This guide is based on source and test inspection; I did not run those checks.
