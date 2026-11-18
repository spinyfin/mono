# PR #1545: Distinguishing share-link failures and enabling recovery

## 1. The problem being solved

A failed results-share request previously always displayed **“This link isn't available.”** That conflated an unusable link with a temporary service or network failure, and provided no retry action.

The decisive old branch treated a missing URL token and every failed view query identically:

```tsx
if (!token || viewQuery.isError) {
  return <UnavailableState />;
}
```

[Before: `ShareResultsPage.tsx`](https://github.com/brianduff/flunge/blob/b4e72c775d4e60b5fe0565599f666fdcd2a598cd/frontend/src/ShareResultsPage.tsx)

This guide covers [PR #1545](https://github.com/brianduff/flunge/pull/1545), comparing base `b4e72c775d4e60b5fe0565599f666fdcd2a598cd` with head `ba2e4fce1b1de11fe605390ed84dfe9f0f415e99`. Links below use those pinned revisions because exact diff anchors were not supplied.

## 2. The core fix

The change separates failure classification, presentation, and query control:

- **`shareLoadFailure.ts`** defines the two failure kinds, their copy, and a classifier.
- **`ShareLoadFailureState`** renders the shared error card and an optional retry button.
- **`ShareResultsPage`** classifies view-query errors and supplies a callback that calls `viewQuery.refetch()` for transient failures.

The resulting page behavior is:

| Failure                                                             | Presentation                                                            | Retry action  |
| ------------------------------------------------------------------- | ----------------------------------------------------------------------- | ------------- |
| Missing URL token; `HttpError` with 4xx other than 408/429          | “This link isn't available.” / “Check the link with whoever shared it.” | None          |
| `HttpError` with 408/429 or any non-4xx status; any non-`HttpError` | “Couldn't load this right now.” / “Try again in a moment.”              | **Try again** |

Sources: [classification and copy](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/shareLoadFailure.ts), [page integration](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/ShareResultsPage.tsx).

This comparison integrates the results recipient page. Reuse by a future plan recipient page is an intention stated in the description and component comment, not an integration implemented here.

## 3. Implementation walkthrough and worked example

**Start with the classifier: it applies a status policy, not a diagnosis of the token.**

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

`TRANSIENT_CLIENT_STATUSES` contains 408 and 429. Checking those exceptions first prevents the general 4xx branch from making them permanent. Everything outside that branch defaults to transient. [Core classifier](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/shareLoadFailure.ts#L19)

The `instanceof HttpError` requirement matters: an arbitrary object with a `status` property does not receive status-based classification. Likewise, “permanent” is a UI policy for these statuses; the classifier does not inspect a response body or establish that a token was specifically revoked or expired. The HTTP helper and share API implementations were not supplied, so their error-conversion behavior cannot be independently verified here.

**Next, follow the page’s error branch and retry transition.**

Missing tokens get a permanent card separately; the existing `enabled: !!token` query setting prevents that view request. For query errors, the new branch is:

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

The nested `isFetching` check ensures that an error state accompanied by an active fetch displays loading rather than the previous failure card. The subsequent existing loading branch also handles `isLoading` or absent data. [Page control flow](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/ShareResultsPage.tsx#L29)

The query retains `retry: false`, and this change adds no polling interval. That disables query failure retries; it does not establish that every future request must come from the button—event-driven invalidation remains present.

**The shared card handles presentation; its caller enforces retry eligibility.**

The component preserves the previous centered layout, `LinkOff` icon, and `StateCard`, adds the explanatory message, and supplies this action:

```tsx
action={
  onRetry ? (
    <Button variant="soft" onClick={onRetry}>
      Try again
    </Button>
  ) : undefined
}
```

The component checks whether a callback exists, not whether `kind` is transient. The results page enforces the permanent/no-retry pairing by withholding that callback. Future callers must do the same. [Shared card](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/components/ShareLoadFailureState.tsx)

**Worked example: a 503 followed by a successful retry.**

This uses the added integration test’s actual token, mocked responses, and `VIEW` fixture:

1. **Input and state:** render `/r/aK9mQx2LpN4rT8wB` with a fresh query client. The first `getShareResultsView` call rejects with `new HttpError("Failed to GET", 503)`.
2. **Before:** once the query entered its error state, the unconditional error branch displayed “This link isn't available.” There was no retry button.
3. **After:** 503 is neither a transient-client exception nor a 4xx status, so the classifier returns `"transient"`. The page displays “Couldn't load this right now.” and supplies the refetch callback.
4. **Recovery:** clicking **Try again** starts a second view request with the same token. The test deliberately leaves its promise pending and verifies “Loading results...” appears while the transient error message disappears.
5. **Observable result:** resolving that promise with `VIEW` renders **“DOE Jane”**. The test asserts exactly two view API calls, both with the original token.

[Worked-example test and fixtures](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/ShareResultsPage.test.tsx)

**Boundary: an event failure is still a separate path.**

The unchanged event query displays “Unable to load this event.” locally. Specifically, an event-level `HttpError(404)` also invalidates the parent view query. It does **not** directly classify the whole link as permanent; the subsequent view request determines the page state. [Existing event-query behavior](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/ShareResultsPage.tsx)

The corresponding test makes **both** the event request and the subsequent view request return 404, then asserts that the permanent card replaces the fencer view with no retry button. That distinction limits what the test proves.

## 4. Tests and supporting infrastructure

**Added tests:**

- [Classifier suite](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/shareLoadFailure.test.ts): ten status cases cover permanent 400/403/404/410 and transient 408/429/500/502/503/504. Another test checks transient fallback for `TypeError`, ordinary `Error`, a string, and `undefined`.
- [Shared-card suite](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/components/ShareLoadFailureState.test.tsx): two tests verify permanent copy without a button, and transient copy with a button that invokes the supplied callback once.
- [Page suite](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/ShareResultsPage.test.tsx): four added scenarios cover 410, a network-style `TypeError`, 503, and successful manual recovery through the pending/loading state.

**Modified or replaced tests:** the old generic-error/unavailable test becomes a specific 404 test, adding subtitle and no-retry assertions while retaining checks against account prompts and revoked/expired wording. The existing event-404 test gains a no-retry assertion. No other tests are removed in the supplied comparison.

**Test support:** the page suite adds React Testing Library’s `act` import and a manually resolved promise to control the retry’s loading phase. It reuses the existing API mocks, `VIEW`/`EVENT` fixtures, router wrapper, and fresh query client with retries disabled. No fixture files, build configuration, or shared test infrastructure change in this comparison.

**Validation and limits:** I reviewed the supplied before/after source and tests; I did not execute them. The author reports passing frontend Bazel tests, the TypeScript/Vite build, repository checks, and live fake-API checks for 404 and 503. The automated tests mock the API boundary: they verify classification and UI recovery, but do not exercise real network error conversion. Missing-token handling and repeated failure after clicking retry are implemented paths without dedicated tests in the supplied suites.
