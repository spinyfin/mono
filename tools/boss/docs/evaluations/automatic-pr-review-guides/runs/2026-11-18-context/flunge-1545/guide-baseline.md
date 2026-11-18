## Orientation

This PR changes how the public results-sharing page (`/r/:token`) explains a failed load. It distinguishes an unavailable link from a temporary service failure and gives recipients a **Try again** button for the latter.

The change is limited to six frontend files: the results page, a new classifier and shared error card, and their tests. The plan recipient page is absent from this revision; reusing the card there remains future integration.

## 1. Problem being solved

Previously, missing tokens and **every** failed view request reached the same branch:

```tsx
if (!token || viewQuery.isError) {
  return <UnavailableState />;
}
```

That card displayed “This link isn't available.” with no supporting message or retry action. A valid link encountering a network failure or HTTP 503 therefore looked like a permanently invalid link.

The existing HTTP layer already provided the information needed to distinguish these cases. [`get()`](/private/tmp/boss-review-guide-corpus-793/checkouts/flunge-1545/frontend/src/api/http.ts:102) throws `HttpError` with the response status for unsuccessful HTTP responses; a rejected `fetch()` propagates its error. The fix uses that existing distinction.

## 2. Core implementation

The new [`classifyShareLoadFailure()`](/private/tmp/boss-review-guide-corpus-793/checkouts/flunge-1545/frontend/src/shareLoadFailure.ts:33) implements this policy:

| Failure                                                     | Classification                           | Recipient experience                                                    |
| ----------------------------------------------------------- | ---------------------------------------- | ----------------------------------------------------------------------- |
| `HttpError` with 400–499, except 408/429                    | Permanent                                | “This link isn't available.” / “Check the link with whoever shared it.” |
| `HttpError` with 408/429 or any other status, including 5xx | Transient                                | “Couldn't load this right now.” / “Try again in a moment.” plus retry   |
| Anything other than `HttpError`                             | Transient                                | Same temporary-failure message and retry                                |
| Missing route token                                         | Permanent, selected directly by the page | Unavailable-link message                                                |

The decisive classification code is small:

```ts
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
```

The order matters: 408 and 429 must escape the general 4xx rule. Ambiguous errors default to transient. Classification uses the actual `HttpError` class, not message matching or any object that happens to contain a `status` property.

## 3. Review walkthrough and worked example

**Start with the page’s error branch.** [`ShareResultsPage.tsx`](/private/tmp/boss-review-guide-corpus-793/checkouts/flunge-1545/frontend/src/ShareResultsPage.tsx:45) now handles missing tokens separately, classifies query failures, and supplies retry only for transient failures:

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

The `isFetching` check is significant: if an errored query is fetching again, the page shows progress instead of continuing to display the previous failure. The ordinary loading branch below it remains in place.

The query still uses `["shareResultsView", token]`, remains disabled without a token, and retains `retry: false`. This PR adds a manual refetch and no polling interval; it does not change React Query’s other refetch defaults.

**Next, inspect the extracted presentation.** [`ShareLoadFailureState.tsx`](/private/tmp/boss-review-guide-corpus-793/checkouts/flunge-1545/frontend/src/components/ShareLoadFailureState.tsx:12) replaces the page-local `UnavailableState`. It preserves the centered layout, `LinkOff` icon, and existing `StateCard`, while selecting centralized copy and using `StateCard`’s existing `action` slot.

One contract worth noticing: the card renders its button whenever `onRetry` exists. The **page** enforces “retry only for transient failures”; the card does not independently enforce that relationship.

**Worked example: HTTP 503, followed by recovery.** This follows the added [retry test](/private/tmp/boss-review-guide-corpus-793/checkouts/flunge-1545/frontend/src/ShareResultsPage.test.tsx:163), using its actual fixtures.

Input and relevant state:

- Route: `/r/aK9mQx2LpN4rT8wB`.
- A fresh query client has no cached view.
- The first `getShareResultsView(token)` rejects with `HttpError("Failed to GET", 503)`.
- The second call stays pending until the test resolves it with `VIEW`, whose `fencerName` is `"DOE Jane"`.

In production, [`getShareResultsView()`](/private/tmp/boss-review-guide-corpus-793/checkouts/flunge-1545/frontend/src/api/v3/share.ts:84) requests `/api/v3/share/results/view`, sending the token in the `x-flunge-share-token` header. The test mocks this function directly.

**Before:** the failed request set `isError`; the unconditional error branch displayed “This link isn't available.” There was no page-provided retry action.

**After:**

1. Status 503 is neither a transient-client exception nor in the 4xx range, so the classifier returns `"transient"`.
2. The page displays the temporary-failure copy and **Try again**.
3. Clicking the button calls `viewQuery.refetch()` for the same token.
4. While that request is pending, “Loading results...” replaces the failure card.
5. Resolving the request with `VIEW` renders “DOE Jane”.

The test verifies both the visible transition and exactly **two calls with the same token**.

A contrasting 404 clarifies the contract: it enters the permanent 4xx branch and gets no retry button. A missing token also gets permanent presentation, without issuing a view request.

**Finally, check the event-error interaction.** The existing [`ShareEventCard` logic](/private/tmp/boss-review-guide-corpus-793/checkouts/flunge-1545/frontend/src/ShareResultsPage.tsx:116) remains unchanged: an event-level 404 invalidates the parent view query. The updated regression test makes the resulting **view request also return 404**, producing the permanent page-wide state. An event 404 alone is not directly classified as a permanent page failure. Other event failures retain the inline “Unable to load this event.” behavior.

## 4. Tests and supporting infrastructure

Coverage is split across three layers:

- **[Classifier tests](/private/tmp/boss-review-guide-corpus-793/checkouts/flunge-1545/frontend/src/shareLoadFailure.test.ts:5):** table-driven checks for permanent 400/403/404/410 and transient 408/429/500/502/503/504. Separate assertions cover `TypeError("Failed to fetch")`, ordinary `Error`, a string, and `undefined`.
- **[Card tests](/private/tmp/boss-review-guide-corpus-793/checkouts/flunge-1545/frontend/src/components/ShareLoadFailureState.test.tsx:5):** verify both messages, absence of retry for the permanent usage, and one callback invocation when the transient button is clicked.
- **[Page tests](/private/tmp/boss-review-guide-corpus-793/checkouts/flunge-1545/frontend/src/ShareResultsPage.test.tsx:120):** replace the old generic-failure expectation with explicit 404 behavior; add 410, network-failure, 503, and successful manual-retry cases. The existing event-404 test gains a no-retry assertion.

There are **no shared test-infrastructure or configuration changes**. The page tests reuse the existing mocked API functions, fresh `QueryClient`, router wrapper, and fixtures. The retry test adds a manually resolved promise and React Testing Library’s `act()` so it can inspect the pending state before completing the request. Existing [Bazel test globs](/private/tmp/boss-review-guide-corpus-793/checkouts/flunge-1545/frontend/BUILD:6) discover the new test files automatically.

I checked the source and merge-base diff; I did not run the tests. The PR description reports successful frontend Bazel tests, a production build, repository checks, and manual 404/503 verification. No screenshots are registered.

https://github.com/brianduff/flunge/pull/1545
