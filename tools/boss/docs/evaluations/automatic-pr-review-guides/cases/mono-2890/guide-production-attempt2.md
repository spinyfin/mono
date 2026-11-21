# PR #2890: Making Boss’s markdown layout work section by section

## 1. Problem being solved

Boss’s document viewer previously rendered ordinary markdown as one `StructuredText` inside eager `VStack`s. Splitting was reserved for explicitly collapsible headings. Consequently, a long document’s off-screen content remained part of the same layout tree as its visible content.

Two additional paths repeated work:

- Each document-body evaluation recomputed the heading split and scanned the source to choose its width.
- Comment and find updates changed `.id(parseVersion)`, recreating the document subtree to refresh highlighting. Comment updates compensated by capturing the scroll offset and restoring it asynchronously. ([Base implementation](https://github.com/spinyfin/mono/blob/6f8ca35153d0fcae9aea5e9852894ea549989ca0/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L423-L547))

This guide covers the complete comparison from base `6f8ca35153d0fcae9aea5e9852894ea549989ca0` to head `12c538d6f328d9f4cbb62c0ef69d6fa9b301500a`. Links below use revision-pinned source locations.

## 2. Core fix / implementation

The central change is to make **heading sections independent layout units that a `LazyVStack` can defer**. Achieving that requires both splitting ordinary documents and exposing each chunk directly to the lazy stack.

Supporting changes keep that structure efficient and preserve highlighting:

- Cache the heading split and document width across ordinary view invalidations.
- Refresh highlights by changing the parser’s input string while retaining view identity.
- Resolve comment anchors against the concatenated chunk text before distributing them to individual parsers. This prevents independent sections from selecting different occurrences of the same comment quote.

This makes rendering lazy, but **does not eliminate all document-wide work**. Initial source processing still builds plain-text projections for every chunk, and `renderedChunks` still constructs parser entries for the whole chunk array. The improvement principally concerns layout granularity, repeated source processing, and subtree recreation. ([Chunk caching and parser construction](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L544-L610))

## 3. Implementation walkthrough and worked example

### Start with the new unit of layout

`MarkdownHeadingSections.chunks` now visits every recognized heading, regardless of whether any headings are collapsible:

```swift
let headings = headingLines(in: source)
guard !headings.isEmpty else { return [.plain(source)] }
```

For each heading, the section ends at the next heading of **any level**. A configured collapsible heading produces a body-only `.collapsible` chunk; otherwise, `.plain` includes both the heading line and its body. Content before the first heading becomes a separate prefix chunk. ([Splitter](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L697-L722))

The existing scanner recognizes ATX headings and skips lines while its fence toggle is active. It is not a general Markdown syntax-tree splitter. Documents without recognized headings still have one potentially large chunk, so the optimization’s granularity depends on document structure. ([Heading recognition](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L724-L776))

### Then expose those units to SwiftUI’s lazy layout

The important hierarchy is now:

```swift
// Condensed structure; chrome and layout modifiers omitted.
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

The `LazyVStack` is the scroll content, and its `ForEach` is not hidden inside another document-body stack or conditional wrapper. Theme application also moves inside the loop. That matters because `.bossMarkdown()` returns a custom `BossMarkdownTheme` view; wrapping the entire loop would group its content behind that view boundary. ([Layout change](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L327-L379), [unchanged theme wrapper](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/BossMarkdownStyle.swift#L870-L892))

Spacing becomes explicit: the header and divider receive 12-point bottom padding, while later plain chunks receive heading top spacing. The latter uses the editorial H2 spacing metric for all ordinary chunk boundaries; collapsible chunks retain their own padding.

Height reporting moves to individual chunks, with preference reduction changed to `max`. Thus, a positive chunk height can trigger the existing timing logs. Those logs now indicate initial chunk layout, not completion of layout for every off-screen section.

### Worked example: the actual 24-section test fixture

**Input and state:** `largeHeadedDocument(sections: 24)` generates a title heading and introductory paragraph, followed by 24 `## Section n` sections. Each contains prose, a nested list, and a fenced Swift block. The lazy-layout test uses a 760 × 400 window, disables comments, leaves the collapsible-heading set empty, and installs an appearance probe. ([Fixture and hosting test](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Tests/BossTests/MarkdownDocumentChromeTests.swift#L633-L689))

**Old behavior:**

1. The empty collapsible-heading set immediately returned `[.plain(source)]`.
2. One `StructuredText` therefore received the title, all 24 sections, and their nested content.
3. The eager document stacks offered no section-level boundary at which to defer off-screen layout. ([Base splitter](https://github.com/spinyfin/mono/blob/6f8ca35153d0fcae9aea5e9852894ea549989ca0/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L627-L651))

**New behavior:**

1. The splitter produces **25 plain chunks**: one title section plus 24 numbered sections. The fenced Swift content remains within its section.
2. Each chunk becomes an independently rendered child of the lazy stack.
3. Once the caches are populated, an ordinary body reevaluation with unchanged source and heading configuration reuses the split and width. Appearance, source changes, and heading-configuration changes refresh those caches; an unavailable or mismatched chunk cache falls back to computing the split. ([Cache lifecycle](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L385-L401), [cache lookup](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L571-L585))

**Observable result checked by the test:** after waiting up to two seconds for an appearance event, the probe must contain at least one chunk and no more than `25 / 2`, or **12**, distinct chunk indices:

```swift
XCTAssertGreaterThan(probe.appeared.count, 0)
XCTAssertLessThanOrEqual(probe.appeared.count, total / 2)
```

This is a regression check against broad initial appearance, not an exact visible-section count or a measured speedup. The loop stops once any appearance occurs; it does not observe the entire two-second interval. ([Assertions](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Tests/BossTests/MarkdownDocumentChromeTests.swift#L654-L666))

### Refresh highlights without recreating the document

The dependency’s `StructuredText` stores its parsed attributed string in `@State` and reparses through `.onChange(of: markup, initial: true)`. Replacing its parser instance alone does not trigger that callback. ([Pinned Textual implementation](https://github.com/gonzalezreal/textual/blob/5b06b811c0f5313b6b84bbef98c635a630638c38/Sources/Textual/StructuredText/StructuredText.swift))

The new wrapper changes the markup using `highlightGeneration`:

```swift
StructuredText(
    MarkdownHighlightRefreshParser.tagged(text, generation: highlightGeneration),
    parser: MarkdownHighlightRefreshParser(inner: parser)
)
```

A nonzero generation appends `\n\n<!--boss-md-refresh:N-->`. The outer parser strips that suffix before invoking the comment/search parser, preserving the underlying source and its offsets. ([Wrapper and nonce parser](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L778-L854))

Comment changes, flash changes, and find navigation increment the generation. This removes the explicit `.id()` remount and its offset-restoration workaround. It still requests reparsing through each instantiated chunk’s changed markup; it does not selectively update only the chunk containing a changed highlight. Find navigation also continues to request an intentional scroll to the match.

### Preserve document-level comment selection across chunk boundaries

`MarkdownCommentAnchorMap` addresses the correctness consequence of independent parsers:

1. Project each chunk’s `renderedText` through `CommentProjection`.
2. Concatenate those projections without adding separators and record their character offsets.
3. Resolve each comment anchor once against that concatenation.
4. Intersect the resolved range with each chunk and construct local anchors for nonempty, non-whitespace fragments.

The decisive overlap calculation is:

```swift
let overlapStart = max(match.lowerBound, chunkStart)
let overlapEnd = min(match.upperBound, chunkEnd)
guard overlapStart < overlapEnd else { continue }
```

Clipped anchors receive local `exact` text and up to 64 characters of prefix/suffix context. Flash anchors follow the same assignment path. ([Projection and assignment](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/Comments/MarkdownCommentAnchorMap.swift#L90-L165))

The existing resolver uses whitespace-tolerant matching, scores surrounding context when multiple candidates exist, and keeps the earliest candidate on a tie. Context is a ranking mechanism, not a required exact match. ([Unchanged resolver](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/Comments/CommentHighlightingParser.swift#L145-L190))

The map has two cache levels: source, heading configuration, base URL, and chunk count determine projection reuse; highlighted and flashing anchors additionally determine assignment reuse. A flash-only update reruns assignment without rebuilding projections. This relies on the caller deriving chunks consistently from source and heading configuration—the cache does not compare every chunk’s text. ([Cache](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/Comments/MarkdownCommentAnchorMap.swift#L28-L69))

Find already operates on per-chunk projections, so its algorithm is unchanged. Unlike comment resolution, it searches each chunk separately; cross-boundary comment support should not be read as cross-boundary find support. Collapsible heading labels remain outside the parser/search corpus. ([Find implementation](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/Search/MarkdownFindState.swift#L104-L182))

## 4. Tests and supporting infrastructure

**Seven tests are added:**

| Area                   | Scenario and assertion                                                                                                                                  |
| ---------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Chunking               | A document without headings remains exactly one plain chunk.                                                                                            |
| Highlight refresh      | Generation zero leaves source unchanged; generation seven changes it; stripping restores the original source.                                           |
| Repeated comment quote | Two sections contain `checkleft run`; prefix/suffix select Alpha, only Alpha receives an anchor, and only one parsed chunk has background highlighting. |
| Cross-heading comment  | An anchor derived from text on both sides of a projection boundary is assigned to both chunks; both parsed results contain highlighting.                |
| Whitespace fragment    | An anchor whose `exact` is `"\n"` fails `isAnchorable`. This tests the predicate directly, not end-to-end clipping.                                     |
| Large-document hosting | The fixture produces 25 chunks and a positive hosting height.                                                                                           |
| Lazy layout            | The short-window appearance probe records between one and 12 distinct chunks at the observation point.                                                  |

See the [no-heading test](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Tests/BossTests/MarkdownDocumentChromeTests.swift#L317-L322) and [new refresh, anchor, and layout tests](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Tests/BossTests/MarkdownDocumentChromeTests.swift#L501-L689).

**Three existing behavioral tests are modified.** The empty and nonmatching collapsible-heading tests are renamed and now expect splitting at ordinary headings; the empty-set case also verifies exact source reconstruction. The fenced-code test now expects two real heading chunks, no collapsible fake heading, and exact reconstruction. The old single-chunk expectations are replaced; no separate test coverage is otherwise removed. Existing collapse-boundary and find-index mapping tests remain. ([Updated chunking tests](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Tests/BossTests/MarkdownDocumentChromeTests.swift#L289-L322), [fence test](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Tests/BossTests/MarkdownDocumentChromeTests.swift#L387-L413))

**Infrastructure additions** are the optional environment-injected `MarkdownChunkAppearProbe`, an AppKit import, the generated large-document fixture, a helper detecting background-color attributes, and an `NSWindow`/run-loop harness for appearance delivery. The probe records `onAppear`, not parser calls or layout passes. No build configuration changes are included; existing source globs include the new production file and keep these tests in `BossTestsShard3`. ([Probe](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L237-L256), [existing Bazel configuration](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/BUILD.bazel))

The main coverage limits are direct scroll preservation during highlight changes, cache reuse/invalidation, flash-anchor behavior, and measured layout cost. The nonce test verifies string handling rather than a mounted view’s refresh lifecycle.

The author reports building `boss_mac_app_lib` and `BossTestsShard3`, running `bazel test //tools/boss/app-macos:BossTestsShard3`, and running `checkleft run`. I inspected the comparison and supporting source; I did not run those checks.
