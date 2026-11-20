1. **Problem being solved**

[PR #1545](https://github.com/brianduff/flunge/pull/1545) fixes misleading error handling on the public results-share page, `/r/:token`. Previously, a missing token, a rejected link, and a temporary backend outage all produced the same message: **“This link isn't available.”** There was no retry button.

The old decision reduced every failed view request to one state:

```tsx
if (!token || viewQuery.isError) {
  return <UnavailableState />;
}
```

That incorrectly suggested a valid link was broken during connection failures or temporary server errors.

2. **Core fix**

The page now classifies failures before choosing its message and action:

| Failure                                             | Message and guidance                                                    | Action          |
| --------------------------------------------------- | ----------------------------------------------------------------------- | --------------- |
| Missing URL token; HTTP 4xx except 408/429          | “This link isn't available.” / “Check the link with whoever shared it.” | No retry button |
| HTTP 408/429, 5xx, network failures, unknown errors | “Couldn't load this right now.” / “Try again in a moment.”              | **Try again**   |

The core policy is in [shareLoadFailure.ts](/Users/brianduff/.local/share/cube/workspaces/mono-agent-807/.codex/corpus-work/checkouts/flunge-1545/frontend/src/shareLoadFailure.ts:33):

```ts
export function classifyShareLoadFailure(error: unknown): ShareLoadFailureKind {
  if (error instanceof HttpError) {
    if (TRANSIENT_CLIENT_STATUSES.has(error.status)) {
      return "transient";
    }
    if (error.status >= 400 && error.status < 500) {
      return "permanent";
    }
    return "transient";
  }
  return "transient";
}
```

`TRANSIENT_CLIENT_STATUSES` contains `408` and `429`. Those exceptions must precede the general 4xx check. Ambiguous failures default to transient.

This uses existing transport behavior: [http.ts](/Users/brianduff/.local/share/cube/workspaces/mono-agent-807/.codex/corpus-work/checkouts/flunge-1545/frontend/src/api/http.ts:102) already throws `HttpError` with the response status for unsuccessful GETs and lets network errors propagate. No backend or HTTP-helper changes are needed. The classifier recognizes actual `HttpError` instances; it does not inspect error-message text or arbitrary objects with a `status` property.

3. **Walkthrough of the logic and architecture**

Start with [ShareResultsPage.tsx](/Users/brianduff/.local/share/cube/workspaces/mono-agent-807/.codex/corpus-work/checkouts/flunge-1545/frontend/src/ShareResultsPage.tsx:29). The query retains its existing key, `["shareResultsView", token]`, its missing-token guard, and `retry: false`. The change adds manual retry without introducing polling.

A missing token explicitly selects the permanent card. For a failed request, the decisive new branch is:

```tsx
if (viewQuery.isError) {
  if (viewQuery.isFetching) {
    return <LoadingState message="Loading results..." />;
  }
  const kind = classifyShareLoadFailure(viewQuery.error);
  return (
    <ShareLoadFailureState
      kind={kind}
      onRetry={kind === "transient" ? () => void viewQuery.refetch() : undefined}
    />
  );
}
```

The `isFetching` check handles an error-state query being fetched again: the page shows progress instead of leaving the previous failure card visible.

Presentation moves from the page’s private `UnavailableState` into [ShareLoadFailureState.tsx](/Users/brianduff/.local/share/cube/workspaces/mono-agent-807/.codex/corpus-work/checkouts/flunge-1545/frontend/src/components/ShareLoadFailureState.tsx:12). It preserves the `LinkOff` icon and centered `StateCard` layout, reads centralized copy, and uses `StateCard`’s existing `action` slot for the button.

One useful review detail: **the caller controls retry eligibility**. The component displays a button whenever `onRetry` is supplied; the page ensures that only transient failures receive that callback. Classification, presentation, and query control therefore have separate responsibilities.

**Worked example: a 503 followed by successful retry.** This comes directly from the [new page test](/Users/brianduff/.local/share/cube/workspaces/mono-agent-807/.codex/corpus-work/checkouts/flunge-1545/frontend/src/ShareResultsPage.test.tsx:163), using its fixture inputs:

- The recipient opens `/r/aK9mQx2LpN4rT8wB` with a fresh query client. The first `getShareResultsView` call rejects with `new HttpError("Failed to GET", 503)`.
- **Before:** `viewQuery.isError` immediately selected the generic unavailable card. The recipient saw “This link isn't available.” and had no in-page retry action.
- **After:** `503` is neither an exceptional client status nor a 4xx status, so the classifier returns `"transient"`. The recipient sees the temporary-failure guidance and **Try again**.
- Clicking the button calls `viewQuery.refetch()` for the same query and token. While the second request is pending, the test observes **“Loading results...”** and verifies that the transient message disappears.
- The test resolves that request with `VIEW`, whose `fencerName` is `"DOE Jane"`. The page renders **DOE Jane** and verifies exactly two view calls, both with the original token. Recovery occurs without a full-page reload.

The contrasting boundary is a `404` or `410`: it remains permanent and offers no retry button.

Event-level failures retain their existing behavior. In [ShareEventCard](/Users/brianduff/.local/share/cube/workspaces/mono-agent-807/.codex/corpus-work/checkouts/flunge-1545/frontend/src/ShareResultsPage.tsx:117), an event `404` invalidates the parent view query. **It is the subsequent view failure that determines the whole-page error.** The regression test makes that view refetch return `404`, then verifies permanent copy and no retry. Other event errors continue to display “Unable to load this event.” within the page.

The reusable card currently serves the results recipient page only. The plan recipient page is absent from this revision; adopting the card there remains follow-up work.

4. **Tests and supporting infrastructure**

| Test area                                                                                                                                                                    | Added or changed coverage                                                                                                                                                                  |
| ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| [Classifier tests](/Users/brianduff/.local/share/cube/workspaces/mono-agent-807/.codex/corpus-work/checkouts/flunge-1545/frontend/src/shareLoadFailure.test.ts:4)            | Permanent: 400/403/404/410. Transient: 408/429/500/502/503/504. Also covers a fetch `TypeError`, generic `Error`, string, and `undefined`.                                                 |
| [Card tests](/Users/brianduff/.local/share/cube/workspaces/mono-agent-807/.codex/corpus-work/checkouts/flunge-1545/frontend/src/components/ShareLoadFailureState.test.tsx:5) | Both sets of copy; permanent rendering without retry; transient button invokes its callback exactly once.                                                                                  |
| Page tests                                                                                                                                                                   | Replaces the generic-failure test with explicit 404 behavior; adds 410, network failure, 503, and successful manual retry; strengthens the event-404 regression with a no-retry assertion. |

The main testing technique added is a **manually resolved promise** for the second view request. Importing React Testing Library’s `act` lets the test resolve it deliberately, checking both the intermediate loading state and successful recovery.

No shared test infrastructure, dependencies, Jest configuration, or Bazel rules changed. The existing router/query-client harness and API mocks are reused; Bazel’s existing test-file glob discovers the new suites.

I inspected the code and tests but did not execute them. The PR description reports passing frontend Bazel tests, the TypeScript/Vite build, repository checks, and manual fake-API checks for 404 and 503.
