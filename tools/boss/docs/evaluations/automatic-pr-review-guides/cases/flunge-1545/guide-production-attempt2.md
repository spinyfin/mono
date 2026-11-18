# PR #1545: Distinguishing temporary share-link failures from unavailable links

## 1. Problem being solved

The results recipient page, `/r/:token`, previously treated every failed view request as evidence that the link was unavailable:

```tsx
if (!token || viewQuery.isError) {
  return <UnavailableState />;
}
```

That [base-revision branch](https://github.com/brianduff/flunge/blob/b4e72c775d4e60b5fe0565599f666fdcd2a598cd/frontend/src/ShareResultsPage.tsx#L45-L47) displayed “This link isn't available.” without a retry action. A network failure or HTTP 503 therefore looked like an invalid or expired link.

This guide covers the complete comparison from `b4e72c775d4e60b5fe0565599f666fdcd2a598cd` to `ba2e4fce1b1de11fe605390ed84dfe9f0f415e99`. Links below use those pinned revisions because exact diff-anchor mappings were unavailable.

## 2. Core fix / implementation

The change separates failure classification, presentation, and request control:

- **`shareLoadFailure.ts`** defines the permanent/transient distinction and corresponding copy.
- **`ShareLoadFailureState`** renders the shared error card and an optional retry action.
- **`ShareResultsPage`** classifies the view-query error and supplies `viewQuery.refetch()` for transient failures.

The resulting [classification and copy](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/shareLoadFailure.ts#L3-L44) are:

| Condition                                                                  | Recipient-facing result                                                                              |
| -------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------- |
| Missing token in the component; `HttpError` with 4xx status except 408/429 | “This link isn't available.” / “Check the link with whoever shared it.” No retry button.             |
| `HttpError` with 408, 429, or any status outside 400–499; any other error  | “Couldn't load this right now.” / “Try again in a moment.” The page supplies a **Try again** button. |

“Permanent” is a frontend policy based on status, not a diagnosis of why a token failed. Likewise, “transient” means retry is appropriate under this policy; it does not guarantee eventual success.

The integration is limited to the results recipient page. The comment about future plan-page reuse expresses intended adoption; this comparison does not implement that integration or change owner-side share dialogs.

## 3. Walkthrough of the logic and architecture

### Follow the existing error path into the new classifier

The unchanged [`getShareResultsView`](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/api/v3/share.ts) requests `/api/v3/share/results/view`, passing the token in `x-flunge-share-token`. The underlying [`get` helper](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/api/http.ts) already throws `HttpError` with the response status for unsuccessful HTTP responses. Fetch rejections propagate without being converted to `HttpError`.

That existing distinction supplies everything the [new classifier](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/shareLoadFailure.ts#L33-L44) needs:

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

`TRANSIENT_CLIENT_STATUSES` contains 408 and 429, so those exceptions must precede the general 4xx check. The `instanceof` requirement matters: the implementation does not inspect arbitrary objects with a `status` field or parse error messages. All non-`HttpError` values take the transient fallback.

### Let the page own retry policy and loading transitions

The [page’s replacement error branch](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/ShareResultsPage.tsx#L45-L64) first handles a missing token as permanent. The existing query also uses `enabled: !!token`, so that case does not issue a view request.

For request failures:

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

The `isFetching` check gives an in-flight refetch precedence over the previous error display. The ordinary `isLoading || !viewQuery.data` branch remains immediately afterward.

The [query configuration](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/ShareResultsPage.tsx#L29-L32) retains `retry: false`; no polling interval is introduced. That disables automatic failure retries, but should not be read as a guarantee that only button clicks can cause future requests: query invalidation remains in use.

### Keep the card presentational

[`ShareLoadFailureState`](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/components/ShareLoadFailureState.tsx#L12-L36) replaces the page-local `UnavailableState`. It preserves the centered layout and `LinkOff` icon, selects copy by `kind`, and uses the existing [`StateCard.action` slot](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/components/PageStates.tsx).

Its button condition is simply:

```tsx
onRetry ? (
  <Button variant="soft" onClick={onRetry}>
    Try again
  </Button>
) : undefined;
```

Thus, **the page enforces “retry only for transient failures.”** The component’s props do not prohibit supplying `onRetry` with `kind="permanent"`.

### Worked example: a 503 followed by recovery

This example uses the actual [retry test’s input and fixture](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/ShareResultsPage.test.tsx#L162-L193):

- Route: `/r/aK9mQx2LpN4rT8wB`.
- A fresh `QueryClient`, with no previously loaded view.
- First `getShareResultsView` call rejects with `new HttpError("Failed to GET", 503)`.
- The second call stays pending until the test resolves it with `VIEW`, whose fencer name is `"DOE Jane"`.

**Old behavior:** the first rejection makes `viewQuery.isError` true. The unconditional error branch renders “This link isn't available.” No retry button exists, so this recovery sequence cannot be initiated through the card.

**New behavior:**

1. The token is present, so the query runs.
2. The classifier receives `HttpError(503)`. It matches neither the 408/429 set nor the 4xx range, and returns `"transient"`.
3. The page renders the temporary-failure copy and supplies a callback to `viewQuery.refetch()`.
4. Clicking **Try again** calls the same view API with the same token. While the second promise is pending, the page displays “Loading results...” and removes the error copy.
5. Resolving that promise with `VIEW` renders `"DOE Jane"`. The test asserts exactly two view calls, both with `"aK9mQx2LpN4rT8wB"`.

The contrasting [404 case](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/ShareResultsPage.test.tsx#L120-L133) takes the permanent branch and exposes no retry button.

### Preserve the distinction between view and event failures

The [unchanged event-query handler](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/ShareResultsPage.tsx#L116-L135) normally reports “Unable to load this event.” inside the event card. Specifically for `HttpError(404)`, it invalidates the parent view query.

An event 404 therefore does **not itself** establish a permanent page failure. In the [existing regression scenario](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/ShareResultsPage.test.tsx#L262-L278), the subsequent **view request also returns 404**. That second failure produces the permanent page card, now explicitly asserted to have no retry button.

## 4. Tests and supporting infrastructure

**Added tests:**

- [`shareLoadFailure.test.ts`](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/shareLoadFailure.test.ts#L5-L25): table cases cover permanent 400/403/404/410 and transient 408/429/500/502/503/504. Separate assertions cover `TypeError`, generic `Error`, a string, and `undefined` as transient.
- [`ShareLoadFailureState.test.tsx`](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/components/ShareLoadFailureState.test.tsx#L5-L23): verifies both sets of copy, no action when rendered as permanent without a callback, and one callback invocation after clicking the transient action.
- [`ShareResultsPage.test.tsx`](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/ShareResultsPage.test.tsx#L135-L193): adds view-level 410, network failure, 503, and the complete retry/loading/success sequence described above.

**Modified tests:** the former generic-error/unavailable test becomes an explicit 404 test, adding the explanatory copy and no-retry assertions while retaining checks against sign-up prompts and revoked/expired wording. The event-404 test gains a no-retry assertion. No test files are removed; the old expectation that a generic `Error` means permanent unavailability is replaced.

**Infrastructure:** changes are local to the page test: an `act` import and a manually resolved promise make the pending retry state observable. Existing `VIEW`/`EVENT` fixtures, API mocks, router helper, and fresh query-client setup are reused. No build or Jest configuration changes occur; the existing [Bazel test-file glob](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/BUILD) discovers the new tests.

**Coverage and validation:** these tests exercise mocked API outcomes, not an actual deployment interruption. They do not add coverage for a failed second attempt or the page’s missing-token branch. I inspected the complete revision diff and relevant helpers/configuration; I did not run tests. The [PR description](https://github.com/brianduff/flunge/pull/1545) reports passing frontend Bazel tests, `npm run build`, `./tools/checks run`, and manual fake-API checks for 404 and 503.
