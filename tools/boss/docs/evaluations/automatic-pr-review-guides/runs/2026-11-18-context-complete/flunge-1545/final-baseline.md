**Orientation — the problem**

PR #1545 changes how the public results page (`/r/:token`) handles a failed load. Previously, every failure produced **“This link isn't available.”** That conflated an invalid share link with a temporary service outage or network failure.

The change gives recipients an appropriate next step: check a permanently unavailable link with its sender, or retry a temporary failure. It touches six frontend files. The plan recipient page is absent from this checkout; the new component is available for future reuse there.

**1. Core implementation**

The fix separates error classification, presentation, and query behavior:

| Failure                                    | Classification                          | Recipient sees                                                                |
| ------------------------------------------ | --------------------------------------- | ----------------------------------------------------------------------------- |
| `HttpError` with 400–499, except 408/429   | Permanent                               | “This link isn't available.” / “Check the link with whoever shared it.”       |
| 408, 429, 5xx, or an unknown/network error | Transient                               | “Couldn't load this right now.” / “Try again in a moment.” plus **Try again** |
| Missing route token                        | Permanent, handled directly by the page | Permanent card; no request                                                    |

The decisive rule is in [shareLoadFailure.ts](/private/tmp/boss-review-guide-corpus-793/checkouts/flunge-1545/frontend/src/shareLoadFailure.ts:34):

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

`TRANSIENT_CLIENT_STATUSES` contains `408` and `429`. The order matters: these exceptions must precede the general 4xx rule.

This is a presentation policy, not a determination of whether a token is actually valid. Only the existing `HttpError` class receives status-based treatment; other error values default to transient.

**2. Walkthrough of the logic and architecture**

Start with the existing transport contract in [api/http.ts](/private/tmp/boss-review-guide-corpus-793/checkouts/flunge-1545/frontend/src/api/http.ts:102). Its `get()` helper already throws `HttpError` with the response status for unsuccessful HTTP responses. A rejected `fetch()` propagates without that wrapping. The PR therefore needs no transport changes.

Next, review [ShareResultsPage.tsx](/private/tmp/boss-review-guide-corpus-793/checkouts/flunge-1545/frontend/src/ShareResultsPage.tsx:45). The old branch discarded the reason for failure:

```tsx
if (!token || viewQuery.isError) {
  return <UnavailableState />;
}
```

The replacement handles a missing token separately, then classifies query failures and supplies a retry callback only for transient ones:

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

The `isFetching` check makes the retry transition visible: an in-flight retry shows **“Loading results...”** instead of retaining the error card. The existing query still has `retry: false`; this adds a manual `refetch()`, with no polling interval.

Finally, [ShareLoadFailureState.tsx](/private/tmp/boss-review-guide-corpus-793/checkouts/flunge-1545/frontend/src/components/ShareLoadFailureState.tsx:12) replaces the page’s private `UnavailableState`. It preserves the centered layout, `LinkOff` icon, and existing `StateCard`, while selecting centralized copy and using `StateCard`’s existing action slot.

One useful ownership detail: **the page enforces which failures get retry**. The component renders a button whenever `onRetry` exists; it does not independently check `kind`.

Event-level handling remains separate. In [ShareEventCard](/private/tmp/boss-review-guide-corpus-793/checkouts/flunge-1545/frontend/src/ShareResultsPage.tsx:117), ordinary event failures still display “Unable to load this event.” An event 404 invalidates the overall view query. The subsequent **view response** determines the page-level classification; an event 404 alone does not force permanent failure.

**3. Worked example: temporary 503, then recovery**

This example uses the actual [retry test’s inputs and fixtures](/private/tmp/boss-review-guide-corpus-793/checkouts/flunge-1545/frontend/src/ShareResultsPage.test.tsx:163).

Input and state:

- Route: `/r/aK9mQx2LpN4rT8wB`.
- A fresh query client has no cached view.
- The first `getShareResultsView(token)` call rejects with `new HttpError("Failed to GET", 503)`.
- The second call remains pending until the test resolves it with `VIEW`, whose `fencerName` is `"DOE Jane"`.

The unchanged [API wrapper](/private/tmp/boss-review-guide-corpus-793/checkouts/flunge-1545/frontend/src/api/v3/share.ts:84) sends the token in `x-flunge-share-token` to `/api/v3/share/results/view`.

**Before:** the first rejection sets the query’s error state. The unconditional `viewQuery.isError` branch renders “This link isn't available.” with an empty supporting message and no retry action.

**After:**

1. The classifier recognizes `HttpError(503)`. It matches neither the transient 4xx exceptions nor the permanent 4xx range, so returns `"transient"`.
2. The page renders “Couldn't load this right now.” and **Try again**.
3. Clicking the button calls `viewQuery.refetch()` for the same token.
4. While the second request is pending, the page shows “Loading results...” and removes the transient error message.
5. Resolving the request with `VIEW` renders **“DOE Jane”**.

The test asserts exactly two view calls, both with the original token. Recovery occurs through the query rather than a page reload.

The contrasting boundary is a view `404` or `410`: both classify as permanent, and the page supplies no retry callback.

**4. Tests and supporting infrastructure**

The coverage follows the same three layers:

- [Classifier tests](/private/tmp/boss-review-guide-corpus-793/checkouts/flunge-1545/frontend/src/shareLoadFailure.test.ts:4) add ten table cases: permanent 400/403/404/410; transient 408/429/500/502/503/504. Another test checks `TypeError`, plain `Error`, a string, and `undefined` as transient.
- [Component tests](/private/tmp/boss-review-guide-corpus-793/checkouts/flunge-1545/frontend/src/components/ShareLoadFailureState.test.tsx:5) verify both messages, absence of retry for the permanent rendering, and one callback invocation when the transient button is clicked.
- [Page tests](/private/tmp/boss-review-guide-corpus-793/checkouts/flunge-1545/frontend/src/ShareResultsPage.test.tsx:120) replace the generic failure test with an explicit 404 case and add 410, network failure, 503, and recovery coverage. The existing event-404 test now also asserts no retry button; its setup makes both the event request and subsequent view request return 404.

There are **no shared test-infrastructure or configuration changes**. The existing router/query-client harness and API mocks are reused. The recovery test adds a manually resolved promise and React Testing Library’s `act()` to inspect the pending state reliably. Existing Bazel test-file globs pick up the new suites.

I inspected the source and merge-base diff; I did not rerun validation. The PR description reports passing frontend tests, build, and repository checks. Missing-token handling is implemented but has no dedicated test in these changed suites.

https://github.com/brianduff/flunge/pull/1545
