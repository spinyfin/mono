# PR #2890: Keep long Markdown documents responsive with lazy section layout

## Orientation

Reading a long document in Boss should not stall whenever the viewer lays out its content, and updating comment or find highlights should preserve the reader’s place. This performance fix changes the shared Markdown document viewer in the macOS app so off-screen sections can wait to be laid out, while highlight updates reuse the existing views.

The viewer divides documents at headings into **chunks**, meaning independently rendered sections. **Lazy layout** means creating section views as they are needed for display. Previously, ordinary documents were rendered as one large piece; the new structure makes section-level laziness available without requiring any sections to be collapsible.

The revision strengthens that implementation by retaining prepared comment text when highlights flash, discarding whitespace-only fragments at section boundaries, and making the lazy-layout test wait for an appearance event before checking for excessive section creation.

## Change overview

The change combines three related improvements:

- Headed documents render as separate sections inside a lazy vertical stack.
- Repeated view updates reuse the heading split and document-width decision.
- Comment and find highlights refresh through the existing rendered views instead of destroying and recreating the document body.

The scope is the shared `MarkdownDocumentChrome` viewer. Existing callers include the [disk document reader](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/DesignRendererView.swift#L164-L175), the [embedded Designs reader](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/DesignsView.swift#L444-L457), and the [string document viewer](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/DesignsView.swift#L590-L598).

At the merge base, an empty collapsible-heading configuration returned [one chunk containing the entire source](https://github.com/spinyfin/mono/blob/6f8ca35153d0fcae9aea5e9852894ea549989ca0/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L627-L648). The document sat inside [ordinary vertical stacks](https://github.com/spinyfin/mono/blob/6f8ca35153d0fcae9aea5e9852894ea549989ca0/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L293-L315), and highlight changes [replaced the document body’s view identity](https://github.com/spinyfin/mono/blob/6f8ca35153d0fcae9aea5e9852894ea549989ca0/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L439-L469). Those are the central behaviors this PR restructures.

## Core implementation

The main path is source → cached heading chunks → per-chunk parsers → independent rendered views.

`StructuredText` is the Textual library view used to display parsed Markdown. The new `DocumentStructuredText` wrapper supplies each chunk to it, while `LazyVStack`, SwiftUI’s on-demand vertical container, controls section view creation.

Comments require a separate mapping step. A **comment anchor** stores selected text plus surrounding text to distinguish repeated occurrences; its fields are [`exact`, `prefix`, and `suffix`](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/Comments/Comment.swift#L82-L94). A **plain-text projection** is the readable text produced by parsing Markdown and extracting its characters, as implemented by [`CommentProjection.plainText`](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/Comments/CommentWire.swift#L311-L322).

The new anchor map resolves comments against the joined chunk projections, then gives each parser only its overlapping fragments. [Parser construction](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L546-L610) layers local comment highlighting beneath local find highlighting.

## Implementation walkthrough

### 1. Make heading sections the layout unit

[`MarkdownHeadingSections.chunks`](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L697-L719) now iterates over every recognized heading. Each section ends at the next heading of any level. Text before the first heading becomes a leading chunk; a document without recognized headings remains one chunk.

The decisive branch preserves ordinary heading text and removes it from the parsed body only for explicitly collapsible sections:

```swift
if collapsibleHeadings.contains(heading.text) {
    let body = String(source[heading.lineRange.upperBound..<sectionEnd])
    chunks.append(.collapsible(heading: heading.text, body: body))
} else {
    chunks.append(.plain(String(source[heading.lineRange.lowerBound..<sectionEnd])))
}
```

The [existing heading scanner](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L735-L775) recognizes hash-prefixed headings and skips fenced code. Collapsible labels remain separate controls; [their parsed text is only the body](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L644-L657).

The viewer [refreshes its chunk and width caches on appearance and source/configuration changes](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L385-L398). [Cache reads](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L571-L585) check source and heading configuration before reusing chunks, falling back to a fresh split when necessary.

### 2. Preserve laziness through the view hierarchy

The [scroll content](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L327-L379) is directly a `LazyVStack`. Its section `ForEach` is a direct child, and errors select an empty section array instead of wrapping the whole section list in a conditional view.

Condensed structure:

```swift
ScrollView {
    LazyVStack(alignment: .leading, spacing: 0) {
        // Header, divider, and optional load error.
        ForEach(loadError == nil ? renderedChunks : []) { entry in
            chunkView(entry)
                .bossMarkdown()
            // Per-chunk environments and height reporting.
        }
    }
}
```

Placement matters because [`bossMarkdown()` introduces a wrapper view](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/BossMarkdownStyle.swift#L870-L892). Applying it to each section keeps the list exposed to the lazy container. Header and divider spacing becomes explicit; [`MarkdownChunkTopSpacing`](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L802-L825) adds the gap above ordinary chunks after the first.

Height reporting also moves to each chunk. The [preference reducer takes the maximum reported height](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L227-L234); the [timing handler](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L412-L425) emits parse/interactive logs after a positive chunk height, when project timing is enabled. This is an initial-layout signal, not a sum of all document heights.

### Worked example: opening a long document, then flashing a comment

The added [large-document fixture](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Tests/BossTests/MarkdownDocumentChromeTests.swift#L669-L688) contains a title, an introduction, and 24 numbered sections with nested lists and code blocks. The [lazy-layout test](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Tests/BossTests/MarkdownDocumentChromeTests.swift#L636-L666) opens it in a 760 × 400 viewport with no collapsible headings.

1. **Before:** the empty heading configuration returns the entire fixture as one chunk. One `StructuredText` receives all 24 sections within the eager stack hierarchy.
2. **After:** the splitter produces 25 chunks: the title/introduction and 24 sections. The lazy container can defer section views beyond the viewport. The test waits up to two seconds for the first appearance, then asserts that more than zero and no more than `25 / 2`—12—distinct chunks have appeared.
3. **Observable result:** the initial display can show the beginning without creating every section view. A document with no recognized headings remains a single rendering unit.

For an illustrative follow-on interaction, suppose the reader has scrolled down and an existing comment flashes, with source and heading configuration unchanged. The [old handler](https://github.com/spinyfin/mono/blob/6f8ca35153d0fcae9aea5e9852894ea549989ca0/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L530-L543) changed the body’s identity, captured the scroll offset, and restored it asynchronously after recreation. The new path reuses the heading split and comment projection, reassigns anchors, and refreshes the existing views through the generation mechanism below. It removes the teardown that required scroll restoration.

### 3. Refresh highlights without replacing view identity

A **highlight generation** is a counter incremented when comments, a flash, or find navigation changes. [Those handlers](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L399-L410) now change that counter instead of `.id()`.

[`DocumentStructuredText`](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L784-L798) passes a tagged source string to the renderer:

```swift
StructuredText(
    MarkdownHighlightRefreshParser.tagged(text, generation: highlightGeneration),
    parser: MarkdownHighlightRefreshParser(inner: parser)
)
```

The [parser adapter](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L833-L852) appends a trailing `<!--boss-md-refresh:…-->` tag for nonzero generations, then strips it before the inner parser runs. The changed input requests another parse while the actual parser sees the original text and offsets.

Expanded collapsible bodies [use the same wrapper](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L879-L892). Find navigation still expands the matching collapsed section and explicitly scrolls toward the match; its position remains an [approximation based on text length](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/Search/MarkdownFindState.swift#L95-L103). The removed offset snapshot/restore helpers are no longer needed for highlight remounts.

### 4. Keep comment matching document-wide

Passing every anchor to every section would independently resolve repeated text in each section. Instead, [`MarkdownCommentAnchorMap`](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/Comments/MarkdownCommentAnchorMap.swift#L92-L150) joins the projections without extra separators, resolves each anchor once globally, and intersects the selected range with each chunk.

The [unchanged resolver](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/Comments/CommentHighlightingParser.swift#L146-L184) scores surrounding context and keeps the earliest candidate on a tie. The map then [rewrites overlapping fragments](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/Comments/MarkdownCommentAnchorMap.swift#L152-L165) into local anchors with up to 64 characters of context on either side. A selection crossing a heading can therefore highlight both pieces; whitespace-only pieces are discarded.

The [map cache](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/Comments/MarkdownCommentAnchorMap.swift#L40-L70) separates expensive projection preparation from anchor assignment. Projection reuse checks source, heading configuration, base URL, and chunk count. Unchanged anchors return the cached assignment; changed comments or flash state reuse the projection and rerun assignment.

Find retains its existing [per-chunk search and cached text](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/Search/MarkdownFindState.swift#L112-L129), with [matches computed separately within each chunk](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/Search/MarkdownFindState.swift#L154-L171). Cross-boundary comment resolution does not make find queries span chunk boundaries.

## Tests and supporting infrastructure

**Modified tests:** The former empty/nonmatching collapsible-heading tests now expect three ordinary chunks and check preserved source text or heading placement. The fenced-code test now expects two real heading chunks, rejects a collapsible chunk for the fenced hash line, and checks exact reassembly. These replace the prior single-chunk expectations; no unrelated tests are removed. See [heading cases](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Tests/BossTests/MarkdownDocumentChromeTests.swift#L289-L320) and [fenced-code assertions](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Tests/BossTests/MarkdownDocumentChromeTests.swift#L391-L412).

**Added tests:**

- [No headings](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Tests/BossTests/MarkdownDocumentChromeTests.swift#L316-L320): two paragraphs remain one unchanged plain chunk.
- [Refresh tag](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Tests/BossTests/MarkdownDocumentChromeTests.swift#L503-L509): generation 7 changes the string and strips back to the original; generation zero and untagged input preserve it.
- [Repeated comment text](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Tests/BossTests/MarkdownDocumentChromeTests.swift#L517-L548): “checkleft run” appears in Alpha and Beta, but context selects Alpha; exactly one chunk receives an anchor and exactly one parsed chunk has a highlight background.
- [Cross-heading selection](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Tests/BossTests/MarkdownDocumentChromeTests.swift#L554-L597): a quote formed from four projected characters on each side of the boundary assigns anchors to both chunks, and both parsed results contain highlighting.
- [Whitespace rejection](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Tests/BossTests/MarkdownDocumentChromeTests.swift#L600-L606): a newline-only anchor fails `isAnchorable`.
- [Large-document hosting and laziness](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Tests/BossTests/MarkdownDocumentChromeTests.swift#L617-L666): the fixture yields 25 chunks and a positive hosting height; the separate window-based test enforces the appearance-count bound described above.

Supporting changes add an AppKit window harness, the generated large-document fixture, and a helper detecting highlight backgrounds. A new optional [appearance probe](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/Sources/MarkdownDocumentChrome.swift#L237-L254) records distinct chunk indices through the wrapper’s appearance callback. The revised probe test pumps the event loop until an appearance or its two-second deadline, then rejects broad instantiation rather than requiring an exact visible count.

No build configuration changes accompany the PR. Existing [source discovery](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/BUILD.bazel#L46-L64) includes the new anchor-map file, and the [third test shard](https://github.com/spinyfin/mono/blob/12c538d6f328d9f4cbb62c0ef69d6fa9b301500a/tools/boss/app-macos/BUILD.bazel#L278-L291) continues to use its existing test-source glob.

## Evidence

The PR description reports the following validation:

- The author ran `bazel build //tools/boss/app-macos:boss_mac_app_lib //tools/boss/app-macos:BossTestsShard3`.
- The author ran `bazel test //tools/boss/app-macos:BossTestsShard3`.
- The author ran `checkleft run`.

No screenshots are registered.
