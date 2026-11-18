**Orientation**

[PR #2890](https://github.com/spinyfin/mono/pull/2890) changes Boss’s shared macOS markdown viewer so headed documents render as independently laid-out sections. This guide compares `6f8ca351` with `12c538d6` and follows the implementation and tests directly.

**1. The problem being solved**

Ordinary documents previously became one large `StructuredText` view. The heading splitter returned immediately unless the caller explicitly requested collapsible sections:

```swift
// Before
guard !collapsibleHeadings.isEmpty else { return [.plain(source)] }
```

That renderer sat inside eager `VStack`s. The layout hierarchy therefore offered no section-level boundary at which SwiftUI could defer off-screen work.

Highlight updates also invalidated too much. Changing comments or search incremented `parseVersion`, which was attached to the entire document body as `.id(parseVersion)`. This forced recreation of its view tree. Comment updates compensated for the resulting scroll reset by capturing the offset and restoring it asynchronously.

The problem has three connected parts: oversized layout units, repeated source processing during view updates, and whole-body recreation for highlight changes.

**2. The core implementation**

The fix combines three changes:

- Split every recognized heading into a separate rendering chunk, regardless of whether anything is collapsible.
- Expose those chunks directly to a `LazyVStack` inside the `ScrollView`.
- Cache source-derived data and refresh highlights without changing view identity.

The decisive layout structure is in [MarkdownDocumentChrome.swift:327](/private/tmp/boss-review-guide-corpus-793/checkouts/mono-2890/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift:327). With unrelated modifiers omitted:

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

The placement matters. A surrounding eager `VStack`, or one theme wrapper around the entire `ForEach`, would hide the individual sections behind a single child. Applying `.bossMarkdown()` per chunk preserves the structure the lazy stack needs.

**Worked example: the actual large-document test fixture**

The fixture contains `# Large document`, introductory prose, and 24 `## Section N` sections. Each section includes a paragraph, nested list, and fenced Swift code. Relevant state: no collapsible headings, comments disabled, and a `760 × 400` viewport.

| Step                       | Before                                                                 | After                                                                            |
| -------------------------- | ---------------------------------------------------------------------- | -------------------------------------------------------------------------------- |
| Split source               | Empty collapsible set returns one `.plain(source)` chunk.              | The title and 24 section headings produce **25 plain chunks**.                   |
| Build render hierarchy     | One `StructuredText` contains the whole document inside eager stacks.  | Each chunk has its own `DocumentStructuredText`, exposed through the lazy stack. |
| Initial layout             | There is no independent off-screen section for the container to defer. | SwiftUI can defer sections outside the initial viewport.                         |
| Ordinary view reevaluation | Width detection runs again; chunk preparation is recomputed.           | Valid cached chunks and document width are reused.                               |

The observable check is [the lazy-stack hosting test:636](/private/tmp/boss-review-guide-corpus-793/checkouts/mono-2890/tools/boss/app-macos/Tests/BossTests/MarkdownDocumentChromeTests.swift:636): it records chunk appearances and asserts **at least one, but no more than 12 of the 25 chunks**, has appeared when checked. That is the test’s acceptance condition, not a measured result from this review or a promise of an exact visible-chunk count.

A useful boundary: a document without recognized ATX headings still becomes one chunk. This optimization does not subdivide an exceptionally long headingless document.

**3. Walkthrough of the major logic and architecture changes**

**Heading splitting now defines rendering granularity.** Start with [MarkdownHeadingSections.chunks:695](/private/tmp/boss-review-guide-corpus-793/checkouts/mono-2890/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift:695). It iterates over every heading instead of only headings selected for collapsing:

```swift
if collapsibleHeadings.contains(heading.text) {
    let body = String(source[heading.lineRange.upperBound..<sectionEnd])
    chunks.append(.collapsible(heading: heading.text, body: body))
} else {
    chunks.append(.plain(String(source[heading.lineRange.lowerBound..<sectionEnd])))
}
```

Every section ends at the next heading of **any level**. Ordinary chunks retain their heading line; collapsible chunks render the heading separately as a disclosure label. Any introductory text becomes a leading plain chunk. The existing scanner continues to ignore headings inside fenced code.

**Caches separate document changes from presentation changes.** In [documentChunks and refreshCachedChunks:571](/private/tmp/boss-review-guide-corpus-793/checkouts/mono-2890/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift:571), the heading split is keyed by source and collapsible-heading configuration. The table-sensitive document width is cached alongside it. These values refresh on appearance and when those inputs change; a fallback computes chunks before the cache is populated.

Parser wrappers are still assembled during view evaluation. The optimization avoids repeatedly splitting the source and scanning it for table width; it does not make every body evaluation free.

**Comment anchors need document-wide resolution despite section-local rendering.** The new [MarkdownCommentAnchorMap.swift:95](/private/tmp/boss-review-guide-corpus-793/checkouts/mono-2890/tools/boss/app-macos/Sources/Comments/MarkdownCommentAnchorMap.swift:95) handles this compatibility requirement:

1. Parse each chunk into its rendered plain-text projection.
2. Concatenate those projections and record each chunk’s character offset.
3. Resolve each comment once against that concatenation using the existing context-aware resolver.
4. Intersect the resolved range with each chunk.
5. Create local anchors with clipped text and up to 64 characters of surrounding context.

The decisive intersection is:

```swift
let overlapStart = max(match.lowerBound, chunkStart)
let overlapEnd = min(match.upperBound, chunkEnd)
guard overlapStart < overlapEnd else { continue }

let local = (overlapStart - chunkStart)..<(overlapEnd - chunkStart)
let clipped = clip(plains[index], to: local)
guard isAnchorable(clipped) else { continue }
```

This preserves one selected occurrence when the same phrase appears in several sections, while allowing a selection crossing a heading boundary to highlight both sections.

For example, the new regression test contains `"checkleft run"` in Alpha and Beta, with an anchor whose suffix is `" here"`. Global resolution selects Alpha; only Alpha receives a local anchor. Passing the original anchor independently to both chunk parsers would highlight both, because each has a single local candidate. For this ordinary document, the old one-chunk renderer already selected Alpha correctly—the new map preserves that behavior while introducing chunking.

The [anchor cache:30](/private/tmp/boss-review-guide-corpus-793/checkouts/mono-2890/tools/boss/app-macos/Sources/Comments/MarkdownCommentAnchorMap.swift:30) separates projection validity from highlight state. Source, heading configuration, base URL, or chunk-count changes invalidate the projection. A comment or flash change reuses it and recomputes assignments; unchanged inputs return the cached assignments.

This also establishes the performance limit: building projections still processes every chunk, even initially with no anchors. Find maintains its own projections. The PR primarily defers layout and avoids repeated work after initialization.

**Highlight refresh preserves view identity.** In [DocumentStructuredText and MarkdownHighlightRefreshParser:783](/private/tmp/boss-review-guide-corpus-793/checkouts/mono-2890/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift:783), `highlightGeneration` replaces `parseVersion`.

The adapter changes the markup string to trigger reparsing, then removes its marker before the real parser sees it:

```swift
static func tagged(_ source: String, generation: Int) -> String {
    guard generation != 0 else { return source }
    return source + "\(marker)\(generation)-->"
}

func attributedString(for input: String) throws -> AttributedString {
    try inner.attributedString(for: Self.stripNonce(from: input))
}
```

The marker is `"\n\n<!--boss-md-refresh:"`. It never reaches the inner parser, preserving text offsets. Comment changes, flashing changes, and find navigation increment the generation without replacing the document body’s identity. Accordingly, the old scroll-offset capture/restore methods disappear from `MarkdownFindBar.swift`. Find navigation still intentionally scrolls toward the current match and expands a collapsed section containing it.

`MarkdownFindState.swift` itself changes only comments. Its existing per-chunk search logic automatically adopts the broader splitting policy through the shared helper.

**Spacing and timing follow the new structure.** The lazy stack uses zero spacing; header and divider retain explicit 12-point bottom padding, and subsequent plain chunks receive heading top spacing. Geometry measurements now attach to individual chunks, with the preference reducer taking their maximum height. A positive chunk height can trigger the existing parse/interactive log; it no longer implies every off-screen section has been laid out.

**4. Tests and supporting infrastructure**

All test changes are in [MarkdownDocumentChromeTests.swift](/private/tmp/boss-review-guide-corpus-793/checkouts/mono-2890/tools/boss/app-macos/Tests/BossTests/MarkdownDocumentChromeTests.swift:286).

| Coverage               | What changed                                                                                                                                                                                                    |
| ---------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Heading splitting      | Existing empty/nonmatching collapsible-set tests now expect normal heading chunks. Added a headingless-document test. Updated the fenced-code test to expect two real sections and exact source reconstruction. |
| Highlight refresh      | Added checks that generation zero leaves source unchanged and generation seven produces a tag that strips back to the original source.                                                                          |
| Comment correctness    | Added repeated-phrase disambiguation and cross-heading selection tests. Both run the resulting chunk parsers and inspect background-color attributes.                                                           |
| Whitespace fragments   | Added a direct check that a newline-only anchor is rejected.                                                                                                                                                    |
| Large-document hosting | Added a 25-chunk fixture and a positive fitting-height assertion.                                                                                                                                               |
| Lazy layout            | Added the window-backed appearance-count test described above.                                                                                                                                                  |

The supporting infrastructure is an optional environment-injected `MarkdownChunkAppearProbe`, recorded from `DocumentStructuredText.onAppear`, plus AppKit hosting/window setup and a shared large-document fixture. The lazy test pumps the run loop until the first appearance or a two-second deadline, and tolerates some preloading rather than requiring an exact count. No build configuration or test-runner files changed.

The tests do **not** directly measure layout duration, count cache rebuilds, or verify scroll-offset preservation after a highlight update. The whitespace test exercises the rejection predicate rather than a complete clipping path, and the nonce test exercises string helpers rather than a mounted refresh.

The PR description reports the app/test build, `BossTestsShard3`, and `checkleft run` as validation. I inspected the implementation and assertions; I did not rerun those commands.

https://github.com/spinyfin/mono/pull/2890
