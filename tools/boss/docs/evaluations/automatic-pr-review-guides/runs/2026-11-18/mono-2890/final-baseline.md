PR [#2890](https://github.com/spinyfin/mono/pull/2890) changes the markdown viewer’s unit of rendering from an entire document to individual heading sections. The main review path is **section splitting → lazy view hierarchy → cache invalidation → highlight refresh and anchor mapping**.

This guide compares `6f8ca351` with `12c538d6`.

**1. The problem being solved**

Most documents previously rendered through one `StructuredText`. Heading splitting existed, but only for explicitly collapsible sections. With the normal empty configuration, the splitter immediately returned the entire source:

```swift
guard !collapsibleHeadings.isEmpty else { return [.plain(source)] }
```

That content lived inside ordinary `VStack`s. Consequently, a long document’s off-screen sections remained part of the same eagerly laid-out markdown tree.

Highlight updates added another source of work: changing comments, flashing an anchor, or navigating find results incremented `parseVersion`, which supplied `.id(parseVersion)` on the document body. This forced recreation of its rendered subtree. Comment updates compensated by capturing the scroll offset and restoring it asynchronously after the remount.

The PR addresses both costs: the size of the layout tree SwiftUI needs immediately, and unnecessary reconstruction during subsequent updates.

**2. The core implementation, with a worked example**

The fix has three cooperating parts:

- Split ordinary documents at every recognized heading.
- Present those sections as separate children of a `LazyVStack` directly inside the `ScrollView`.
- Preserve the sections and their view identities across ordinary invalidations and highlight changes.

The decisive layout structure is in [MarkdownDocumentChrome.swift, line 327](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L327). Abbreviating the styling and geometry modifiers:

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

Both levels matter. A lazy stack cannot defer sections if the entire document is still one child. Conversely, splitting the source would not make an ordinary stack lazy.

For a concrete example, use the actual `largeHeadedDocument(sections: 24)` test fixture. It contains:

- `# Large document`, followed by an introductory paragraph.
- Twenty-four `## Section N` headings.
- A paragraph, nested list, and fenced Swift block in each section.

The lazy-layout test supplies this source with `commentsEnabled: false`, the default empty collapsible-heading set, and a **760 × 400** viewport.

| Step                                                    | Before                                                                       | After                                                                                           |
| ------------------------------------------------------- | ---------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------- |
| Split source                                            | Empty collapsible set returns one `.plain(source)`.                          | Recognizes the title plus 24 section headings, producing **25 plain chunks**.                   |
| Construct renderers                                     | One `StructuredText` contains the whole document.                            | Each chunk has its own `DocumentStructuredText`.                                                |
| Lay out the initial viewport                            | The eager hierarchy reaches the whole markdown document.                     | The lazy hierarchy can defer distant sections.                                                  |
| Reevaluate the view with unchanged source/configuration | Recomputes the rendering inputs; the document remains one large layout unit. | Reuses cached chunks and document width; instantiated sections remain independent layout units. |

The observable regression check is deliberately structural, not a stopwatch benchmark: the new appearance probe must record **at least one and at most 12 of the 25 chunks** during the test’s observation window. The code does not specify an exact number of initially instantiated sections.

A useful boundary: a document with **no recognized ATX headings** still becomes one chunk. This optimization therefore depends on heading boundaries; it does not subdivide an exceptionally long heading-free document.

**3. Major logic and architecture changes**

Start with [the splitter, line 696](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L696). It now iterates over **all** headings instead of just headings named in `collapsibleHeadings`:

```swift
let sectionEnd = (i + 1 < headings.count)
    ? headings[i + 1].lineRange.lowerBound
    : source.endIndex

if collapsibleHeadings.contains(heading.text) {
    let body = String(source[heading.lineRange.upperBound..<sectionEnd])
    chunks.append(.collapsible(heading: heading.text, body: body))
} else {
    chunks.append(.plain(String(source[heading.lineRange.lowerBound..<sectionEnd])))
}
```

Ordinary chunks retain their heading line. Collapsible chunks still render the heading separately as a disclosure label and send only the body through the markdown parser. Leading text before the first heading remains its own chunk. Sections end at the next heading of **any level**; fence-aware heading detection is retained.

Next, inspect the view hierarchy. The `ForEach` stays directly inside the lazy stack, and `.bossMarkdown()` moves onto each chunk. This matters because that modifier creates a `BossMarkdownTheme` wrapper: wrapping the entire collection would conceal the individual rows behind one child.

Spacing is now explicit. The header and divider receive 12-point bottom padding; subsequent plain chunks receive heading top spacing through `MarkdownChunkTopSpacing`. Geometry reporting also moves onto individual chunks. `StructuredTextHeightKey` reduces with `max`, so any positive chunk height can trigger the existing timing logs. Those logs now indicate initial chunk layout, not completion of layout for every section.

The next stop is [chunk caching, line 549](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L549). `documentChunks` reuses cached chunks when source and collapsible-heading configuration match. Appearance and changes to either input refresh the cache, including the table-dependent document width. Before the cache is populated—or when its inputs differ—the getter computes the correct split directly.

Parser configurations are still assembled for every chunk. Also, plain-text projections still process the document when their caches are built. **The improvement is deferred rendering/layout and reuse of derived data; it does not eliminate all whole-document processing.**

Highlight refresh is handled by [DocumentStructuredText and MarkdownHighlightRefreshParser, line 778](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L778):

```swift
StructuredText(
    MarkdownHighlightRefreshParser.tagged(text, generation: highlightGeneration),
    parser: MarkdownHighlightRefreshParser(inner: parser)
)
```

The generation changes the markup string by appending a trailing marker such as:

```text
\n\n<!--boss-md-refresh:7-->
```

The wrapper strips that marker before calling the actual parser. This uses `StructuredText`’s markup-change refresh path while retaining SwiftUI identity. It also keeps comment/search offsets based on the real text.

Accordingly, `.id(parseVersion)`, the scroll-offset preservation helper, and `MarkdownScrollController.currentOffset`/`restoreOffset` disappear. Find navigation still deliberately scrolls to its approximate target and expands a collapsed section containing the current match.

Finally, review the new [MarkdownCommentAnchorMap.swift](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/Comments/MarkdownCommentAnchorMap.swift#L13). More rendering chunks introduce a correctness problem: passing every document anchor to every parser could highlight repeated text once per section, while a quote spanning sections would exist in no individual parser’s input.

The new mapping:

1. Projects each chunk into rendered plain text and concatenates those projections.
2. Resolves each anchor once globally using the existing context-sensitive resolver.
3. Intersects that range with each chunk.
4. Creates local anchors with clipped text and up to 64 characters of surrounding context.

The overlap calculation is:

```swift
let overlapStart = max(match.lowerBound, chunkStart)
let overlapEnd = min(match.upperBound, chunkEnd)
guard overlapStart < overlapEnd else { continue }
```

Whitespace-only fragments are discarded before reaching the local highlighter.

Its cache separates **projection validity** from **anchor assignment validity**. Source, heading configuration, base URL, and chunk count determine whether the projection can be reused. Changing highlighted or flashing anchors recomputes assignments against that existing projection.

`MarkdownFindState.swift` itself has only documentation changes. Its existing per-chunk search machinery automatically adopts the expanded splitting behavior through the shared helper.

**4. Tests added and supporting infrastructure**

All test changes are in [MarkdownDocumentChromeTests.swift](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Tests/BossTests/MarkdownDocumentChromeTests.swift#L289).

| Coverage                 | What the assertions establish                                                                                                                   |
| ------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------- |
| Updated splitting tests  | Empty or nonmatching collapsible-heading sets still split real headings; ordinary chunks reconstruct the source exactly.                        |
| Heading-free boundary    | Plain prose remains one unchanged chunk.                                                                                                        |
| Updated fenced-code test | A `#` line inside a fence does not create a section; real headings still do.                                                                    |
| Refresh marker           | Generation zero leaves source unchanged; a nonzero tag changes the string and strips back to the original.                                      |
| Repeated comment text    | An anchor for `checkleft run`, disambiguated by context, reaches only the Alpha section; actual attributed output highlights exactly one chunk. |
| Cross-heading comment    | A quote constructed across the projection boundary is assigned to both overlapping chunks, and both parsed outputs contain highlighting.        |
| Whitespace fragment      | `isAnchorable` rejects a newline-only fragment.                                                                                                 |
| Large-document hosting   | The fixture produces 25 chunks and successfully lays out in an `NSHostingView`.                                                                 |
| Lazy instantiation       | The short viewport reports some chunk appearances without broadly instantiating the document.                                                   |

The supporting infrastructure is a new optional `MarkdownChunkAppearProbe` environment value. `DocumentStructuredText.onAppear` records chunk indices into a set. The test hosts the view in an `NSWindow`, pumps the run loop until the first appearance or a two-second deadline, and checks the broad count bound. It does not require an exact prefetch count.

There are no build-system changes. Existing find-index and collapsed-section tests remain in place. The new tests do **not** directly measure cache reuse, end-to-end scroll preservation, or a timing improvement.

The PR description reports successful Bazel builds, `BossTestsShard3`, and `checkleft run`. I inspected the code and tests; I did not rerun those commands.
