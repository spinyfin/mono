**1. Problem being solved**

A recipient opening a results share link previously saw **“This link isn’t available.” for every view-load failure**. That included temporary service outages, where the link could still be valid.

The old page combined missing tokens and all query errors into one branch:

```tsx
if (!token || viewQuery.isError) {
  return <UnavailableState />;
}
```

That card offered no retry action. A deployment-related 503 or network failure therefore looked like an invalid link. See the [original page implementation](https://github.com/brianduff/flunge/blob/b4e72c775d4e60b5fe0565599f666fdcd2a598cd/frontend/src/ShareResultsPage.tsx).

**2. Core fix / implementation**

The change separates **failure classification**, **error presentation**, and **query recovery**:

- `shareLoadFailure.ts` determines whether the failure is permanent or transient and holds the corresponding copy.
- `ShareLoadFailureState` renders that copy and an optional retry button.
- `ShareResultsPage` classifies view-query errors and supplies `refetch()` only for transient failures.

The resulting behavior is:

| Failure                                                             | Recipient sees                                                          | Action          |
| ------------------------------------------------------------------- | ----------------------------------------------------------------------- | --------------- |
| Missing URL token; `HttpError` with a 4xx status other than 408/429 | “This link isn’t available.” / “Check the link with whoever shared it.” | No retry button |
| 408/429; 5xx; network or unknown error                              | “Couldn’t load this right now.” / “Try again in a moment.”              | **Try again**   |

The decisive classifier is small:

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

`TRANSIENT_CLIENT_STATUSES` contains 408 and 429. The implementation deliberately defaults to transient: only recognized `HttpError` instances in the remaining 4xx range become permanent. It does not inspect error-message text or read a `status` property from arbitrary objects. See [classification and copy](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/shareLoadFailure.ts).

**3. Major logic and architecture changes, with a worked example**

The main review point is the page’s replacement error branch:

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

The `isFetching` check handles a request in progress while the query still reports an error. It gives the recipient visible loading feedback instead of leaving the failure card onscreen. Missing tokens have their own earlier permanent-failure branch; the query remains disabled when there is no token. The existing `retry: false` setting remains, and this change adds no polling. See [page integration](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/ShareResultsPage.tsx).

**Worked example: a 503 followed by a successful retry.** This uses the actual [page test’s inputs and response sequence](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/ShareResultsPage.test.tsx), rather than invented data.

1. **Input and state:** Open `/r/aK9mQx2LpN4rT8wB` with a fresh query client. The token enables the query keyed by `["shareResultsView", token]`. Its first `getShareResultsView(token)` call rejects with `new HttpError("Failed to GET", 503)`.
2. **Old behavior:** `viewQuery.isError` selects `UnavailableState` without examining the error. The recipient sees “This link isn’t available.” and has no in-page retry action.
3. **New behavior:** Once the failed request is no longer fetching, the classifier checks 503. It is neither 408/429 nor in the 400–499 range, so it returns `"transient"`. The page displays temporary-failure copy and supplies a retry callback.
4. **Retry:** Clicking **Try again** calls `viewQuery.refetch()`. The test deliberately leaves this second request pending and verifies that “Loading results...” appears while the transient error message disappears.
5. **Observable result:** The test resolves the pending request with `VIEW`, whose `fencerName` is `"DOE Jane"`. The page renders that name. It verifies exactly two view API calls, both with the same token.

A useful contrasting case is **410**: it falls into the permanent 4xx branch, so the page shows unavailable-link copy without a retry button. The page test explicitly covers this.

The presentation extraction preserves the old `LinkOff` icon, centered placement, and `StateCard` layout. One architectural detail matters for future callers: **the component shows the button whenever `onRetry` is provided; it does not independently enforce `kind === "transient"`**. The page currently enforces that policy. See [the shared card](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/components/ShareLoadFailureState.tsx).

Event-level handling remains separate. An ordinary event failure still shows “Unable to load this event.” within the results page. An event `HttpError(404)` still invalidates the parent view query. In the regression test, **that subsequent view request also returns 404**, producing the permanent page-level state; the event error alone does not directly select that state. See [event handling](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/ShareResultsPage.tsx).

The PR description identifies the shared card as the future integration point for the plan share page. This PR wires it into the results share page only.

**4. Tests added and test infrastructure changes**

Coverage is split across three layers:

- **Classifier tests:** A table covers permanent 400/403/404/410 and transient 408/429/500/502/503/504. Additional assertions cover `TypeError`, generic `Error`, a string, and `undefined`, all transient. See [classifier tests](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/shareLoadFailure.test.ts).
- **Component tests:** Verify permanent copy without a retry button, and transient copy with a button that invokes `onRetry` exactly once. See [card tests](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/components/ShareLoadFailureState.test.tsx).
- **Page tests:** Replace the old generic-error expectation with explicit 404 behavior; add 410, network failure, 503, and the complete retry-to-success flow. The existing event-404 regression gains a no-retry-button assertion. See [page tests](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/ShareResultsPage.test.tsx).

There are **no test infrastructure or configuration changes in the supplied diff**. The existing query/router harness and API mocks are reused. The retry test adds `act` and a manually resolved promise so it can verify the intermediate loading state before completing the request.

The PR description reports passing Bazel frontend tests, the frontend build, repository checks, and manual 404/503 checks against a fake API. Those are reported validation results; I have not rerun them.
