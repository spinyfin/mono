1. **The problem: layout work scaled with the entire document.**

   Most markdown documents previously became one `StructuredText`: `MarkdownHeadingSections.chunks` immediately returned `.plain(source)` when no collapsible headings were configured. That renderer sat inside ordinary `VStack`s, leaving no section boundaries at which SwiftUI could defer off-screen layout.

   Two related costs compounded this:

   - View evaluations recomputed the chunk split and scanned the source to choose the document width.
   - Comment and find updates changed `.id(parseVersion)`, recreating the document’s rendered subtree. Comment updates compensated by saving and asynchronously restoring the scroll offset.

   The relevant starting points are the old [`chunks`, `documentBody`, and `bumpParseVersionPreservingScroll`](https://github.com/spinyfin/mono/blob/6f8ca35153d0fcae9aea5e9852894ea549989ca0/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift).

2. **The core implementation: make heading sections independently renderable and genuinely lazy.**

   Every recognized heading now starts a chunk, regardless of whether that heading is collapsible. Each ordinary chunk includes its heading and everything up to the next heading of **any level**. Content before the first heading becomes a separate prefix chunk.

   The other essential change is the view hierarchy. A simplified excerpt shows the arrangement:

   ```swift
   ScrollView {
       LazyVStack(alignment: .leading, spacing: 0) {
           // Header, divider, and optional error view...
           ForEach(loadError == nil ? renderedChunks : []) { entry in
               chunkView(entry)
                   .bossMarkdown()
                   // Per-chunk environment and geometry modifiers...
           }
       }
   }
   ```

   The placement matters: the lazy stack is the scroll view’s document content, and `ForEach` is its direct child. Applying the theme to each chunk avoids wrapping the entire collection into one child that defeats lazy rendering. See [`MarkdownDocumentColumn.body` and `chunkView`](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift).

   **Worked example — the actual large-document test fixture.** The fixture generates a title and intro, followed by 24 sections. Each section contains a paragraph, a nested list, and a fenced Swift code block. Its beginning looks like this, with the remaining sections omitted:

   ````markdown
   # Large document

   Intro paragraph with **bold** and a [link](https://example.com).

   ## Section 1

   Paragraph 1 with nested structure:

   - top
     - nested
       - deeper still, item 1

   ```swift
   let value = 1
   ```
   ````

   Relevant state: `collapsedByDefaultHeadings` is empty, comments are disabled, and the laziness test uses a **760 × 400** viewport.

   | Step               | Before                                                                          | After                                                                                                                 |
   | ------------------ | ------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------- |
   | Split source       | The empty collapsible set triggers an immediate return of one `.plain(source)`. | The heading scanner finds the title plus 24 section headings, producing **25 plain chunks**.                          |
   | Build the document | One `StructuredText` contains all 24 sections inside eager stacks.              | Each chunk can have its own `DocumentStructuredText` under the direct-child `LazyVStack`.                             |
   | Initial layout     | There is no section-level opportunity to defer the later sections.              | SwiftUI can defer off-screen sections while rendering the opening content.                                            |
   | Observable check   | The old splitter would fail the new 25-chunk assertion.                         | The hosting probe requires **at least one and at most 12** distinct chunks to have appeared at its observation point. |

   That last assertion is evidence of limited initial appearance, **not a measured speedup or an exact count of visible sections**. The fixture and assertions are in [`testLargeDocumentChunksAndHosts` and `testLargeDocumentLazyStackInstantiatesOnlyOnscreenChunks`](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Tests/BossTests/MarkdownDocumentChromeTests.swift).

   The useful boundary is a document without recognized ATX headings: it still becomes one chunk. This optimization therefore depends on heading structure; it does not subdivide an arbitrarily large heading-free section.

3. **The major supporting changes preserve highlighting, caching, and document presentation.**

   **Chunking and width selection are cached.** The column stores the chunks alongside the source and collapsible-heading configuration that produced them. It refreshes these on appearance and when either input changes, also caching the table-dependent document width. `documentChunks` checks those keys before returning cached data and computes a fallback if they do not match. Parser wrappers are still constructed for all chunk entries during view evaluation; the deferred work is the rendering and layout of the chunk views. See [`documentChunks`, `refreshCachedChunks`, and `renderedChunks`](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift).

   **Highlight changes trigger parsing without changing view identity.** `StructuredText` responds to changes in its markup string, so changing the parser alone is insufficient. The new wrapper changes that string using a generation suffix:

   ```swift
   StructuredText(
       MarkdownHighlightRefreshParser.tagged(text, generation: highlightGeneration),
       parser: MarkdownHighlightRefreshParser(inner: parser)
   )
   ```

   Before forwarding to the actual markdown/highlight parser, the wrapper removes the suffix:

   ```swift
   func attributedString(for input: String) throws -> AttributedString {
       try inner.attributedString(for: Self.stripNonce(from: input))
   }
   ```

   For example, generation `7` appends `"\n\n<!--boss-md-refresh:7-->"`. The underlying parser receives the original source, keeping highlight offsets aligned. Comment changes, flash changes, and find navigation increment the generation. Removing `.id(parseVersion)` eliminates the deliberate subtree teardown and its scroll-restoration workaround. This still requests re-parsing; it does not mutate highlights without parsing. See [`DocumentStructuredText` and `MarkdownHighlightRefreshParser`](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift).

   **Comment anchors resolve globally before being assigned locally.** Generalizing chunking creates a correctness problem: passing every comment anchor to every section could highlight repeated text in multiple sections, while a quote crossing a section boundary would exist in no individual chunk.

   The new [`MarkdownCommentAnchorMap`](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/Comments/MarkdownCommentAnchorMap.swift) addresses this by:

   - Projecting each chunk’s rendered markdown into plain text and concatenating those projections.
   - Resolving each anchor once against that concatenation.
   - Intersecting the resolved range with each chunk’s character range.
   - Rebuilding local `exact`, `prefix`, and `suffix` values for overlapping chunks, using up to 64 characters of surrounding context.

   The decisive clipping calculation is:

   ```swift
   let overlapStart = max(match.lowerBound, chunkStart)
   let overlapEnd = min(match.upperBound, chunkEnd)
   guard overlapStart < overlapEnd else { continue }
   ```

   Whitespace-only fragments are discarded so an isolated boundary newline cannot become an ambiguous local highlight. Flashing anchors follow the same assignment path.

   The cache separates the **plain-text projection** from **anchor assignments**. Source, heading configuration, base URL, or chunk-count changes invalidate the projection; changing comments or the flashing anchor reuses it and only recomputes assignments. Initial projection building still processes every chunk, so the change does not eliminate all document-wide parsing.

   **Search and presentation adapt to the new boundaries.** `MarkdownFindState` already searched per chunk; its changes here are documentation-only. The column now refreshes search state when collapsible-heading configuration changes. Existing match navigation and automatic expansion remain in place.

   Chunk boundaries also require explicit spacing: ordinary chunks after the first receive heading top padding, while header and divider retain explicit 12-point bottom spacing. Height preferences now come from individual chunks and reduce using `max`. Consequently, the timing logs can fire when a rendered chunk has positive height; they no longer establish that the entire document has been laid out.

4. **Tests cover the new split contract, comment correctness, and initial laziness.**

   All test changes are in [`MarkdownDocumentChromeTests.swift`](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Tests/BossTests/MarkdownDocumentChromeTests.swift).

   | Area                   | Added or modified coverage                                                                                                                                                                    |
   | ---------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
   | Heading splitting      | Empty and nonmatching collapsible sets now expect separate heading chunks. Tests check exact source reconstruction, preserve fenced-code handling, and add the no-headings/single-chunk case. |
   | Highlight refresh      | Checks that tagging changes the input, stripping restores the source, and generation zero leaves it unchanged.                                                                                |
   | Repeated comment text  | Two sections contain `checkleft run`; context selects Alpha. The test checks both assignment to exactly one chunk and actual background highlighting in exactly one parsed chunk.             |
   | Cross-heading comments | Constructs a selection spanning four projected characters on either side of a boundary, then checks assignment and actual highlighting in both chunks.                                        |
   | Whitespace fragments   | Directly checks that an anchor containing only `"\n"` is rejected by `isAnchorable`.                                                                                                          |
   | Large-document hosting | Checks 25 chunks and successful hosting with a positive fitting height.                                                                                                                       |
   | Lazy appearance        | Checks that a short viewport produces some appearances without broadly instantiating the document.                                                                                            |

   The supporting infrastructure is small: an optional `MarkdownChunkAppearProbe` travels through the SwiftUI environment, and `DocumentStructuredText.onAppear` records its index. Production defaults to no probe. The test imports AppKit, attaches an `NSHostingView` to an `NSWindow`, and pumps the run loop until the first appearance or a two-second deadline. Its generous half-document threshold avoids depending on SwiftUI’s exact prefetch behavior.

   Coverage limits worth keeping in mind during review: the new tests do not directly assert scroll-offset preservation, cache reuse, or elapsed-time improvement. The whitespace test exercises the rejection predicate rather than the complete clipping pipeline.

   The supplied PR description reports successful Bazel builds, `BossTestsShard3`, and `checkleft run`; I have not independently rerun them.
