# PR #1545: Distinguish temporary share-link outages from unavailable links

## Orientation

Someone opening a shared Flunge results link could see “This link isn't available” during a temporary service interruption, even when the link was valid. This bug fix gives recipients an accurate temporary-failure message and a **Try again** button, while keeping the unavailable-link message for failures classified as permanent.

The change lives in the frontend page that displays a fencer’s shared competition results. A **token** is the identifying value carried in the shared link; the **results view** is the initial response containing the fencer’s name and tournament list. The page now distinguishes failures using the server’s response status, treating network failures and unknown errors as temporary.

## Change overview

Previously, a missing token and every failed results-view request reached the [same unavailable state](https://github.com/brianduff/flunge/blob/b4e72c775d4e60b5fe0565599f666fdcd2a598cd/frontend/src/ShareResultsPage.tsx#L45-L47). The new [classification and copy](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/shareLoadFailure.ts#L3-L44) produce these outcomes:

| Failure                                                  | Recipient sees                                                          | Action          |
| -------------------------------------------------------- | ----------------------------------------------------------------------- | --------------- |
| Missing token, or HTTP 400–499 except 408/429            | “This link isn't available.” / “Check the link with whoever shared it.” | No retry button |
| HTTP 408/429, server failures, or network/unknown errors | “Couldn't load this right now.” / “Try again in a moment.”              | **Try again**   |

**HTTP status** means the numeric response code returned by the server. Here, “permanent” is the frontend’s presentation classification; the classifier does not inspect a response body to establish the specific reason a link failed.

The button repeats the results-view request without reloading the page. Automatic failure retries remain disabled, and the change introduces no polling interval.

## Core implementation

The implementation separates three responsibilities:

- [`shareLoadFailure.ts`](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/shareLoadFailure.ts#L3-L44) defines the two failure kinds, their copy, and the classification rule.
- [`ShareLoadFailureState`](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/components/ShareLoadFailureState.tsx#L12-L36) displays the chosen copy and an optional action.
- [`ShareResultsPage`](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/ShareResultsPage.tsx#L24-L64) owns request state and supplies the retry action only for transient failures.

The card retains the broken-link icon and centered placement. It uses the existing [`StateCard`](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/components/PageStates.tsx#L33-L66), a presentation component with slots for an icon, title, message, and action.

The PR description identifies this card as the intended shared error presentation for the upcoming plan recipient page in PR #1542. That integration is future work; this change wires the results recipient page.

## Implementation walkthrough

### 1. Preserve the request’s existing error information

The public [`/r/:token` route](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/main.tsx#L182-L187) renders the results page. Its existing [request helper](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/api/v3/share.ts#L79-L91) requests `/api/v3/share/results/view`, passing the token through a request header.

[`HttpError`](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/api/http.ts#L7-L16) is the existing error type that retains an HTTP response status. The [GET helper](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/api/http.ts#L102-L115) throws it for unsuccessful responses. Network failures propagate from the underlying [`fetch`](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/api/http.ts#L56-L69). No transport change is needed.

### 2. Classify narrowly, with a transient fallback

The [classifier](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/shareLoadFailure.ts#L19-L44) checks the two temporary client-response exceptions before the general 4xx rule:

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

`TRANSIENT_CLIENT_STATUSES` contains 408 and 429. Only an actual `HttpError` enters the status-based branch; every other error is transient. The implementation therefore defaults ambiguous failures to the recoverable presentation rather than parsing error messages.

### 3. Connect classification to loading and retry

**React Query** is the library tracking the request’s data, loading state, and errors. The existing [view query](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/ShareResultsPage.tsx#L29-L32) is enabled only when a token exists and retains `retry: false`.

The page [handles a missing token separately](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/ShareResultsPage.tsx#L45-L64), then processes request failures:

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

`refetch()` repeats that query’s request. The fetching check ensures that a retry in progress displays loading feedback even while the previous error remains present.

The [card renders its button whenever `onRetry` is supplied](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/components/ShareLoadFailureState.tsx#L19-L32). The page, rather than the card’s `kind` property alone, enforces the rule that permanent failures have no retry action.

### 4. Worked example: a 503 followed by recovery

The [retry test](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/ShareResultsPage.test.tsx#L163-L190) supplies a concrete scenario: the recipient opens `/r/aK9mQx2LpN4rT8wB`, the first request rejects with HTTP 503, and the second request remains pending until the test resolves it with the [“DOE Jane” results fixture](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/ShareResultsPage.test.tsx#L35-L49).

1. **Before:** the failed request satisfied `viewQuery.isError`, so the [old branch](https://github.com/brianduff/flunge/blob/b4e72c775d4e60b5fe0565599f666fdcd2a598cd/frontend/src/ShareResultsPage.tsx#L45-L47) displayed the [generic card without an action](https://github.com/brianduff/flunge/blob/b4e72c775d4e60b5fe0565599f666fdcd2a598cd/frontend/src/ShareResultsPage.tsx#L79-L86).
2. **Now:** 503 falls outside the permanent 4xx range, so the page shows the transient copy and **Try again**.
3. Clicking the button issues the request again with the same token. While it is pending, “Loading results...” replaces the failure card.
4. When the response arrives, “DOE Jane” appears. The test asserts exactly two view calls, both with the original token.

A useful boundary is an event-detail failure after the initial results have loaded. An event is one competition within a tournament, loaded separately. The unchanged [event error handler](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/ShareResultsPage.tsx#L116-L135) invalidates the results-view query on an event HTTP 404, causing it to be fetched again. It does not directly classify the whole link as permanent. In the [regression test](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/ShareResultsPage.test.tsx#L263-L276), that follow-up view request also returns 404, which produces the permanent card and hides the previously displayed fencer.

## Tests and test infrastructure

**Added tests:**

- The [classifier suite](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/shareLoadFailure.test.ts#L4-L26) checks permanent classification for 400/403/404/410 and transient classification for 408/429/500/502/503/504. It also checks a network `TypeError`, ordinary `Error`, string, and `undefined`.
- The [card suite](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/components/ShareLoadFailureState.test.tsx#L5-L23) checks permanent copy with no button, transient copy, and one callback invocation per click.
- The [page additions](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/ShareResultsPage.test.tsx#L134-L190) cover 410 without retry, network failure and 503 with retry, and recovery through the loading state to rendered results.

**Modified tests:** the former generic failure test becomes an [explicit 404 test](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/ShareResultsPage.test.tsx#L120-L132), adding the explanatory copy and absent-button assertions while retaining checks against account prompts and revoked/expired wording. The event-404 regression gains the [no-retry assertion](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/ShareResultsPage.test.tsx#L263-L276). No tests are removed outright.

The existing [router/query harness](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/ShareResultsPage.test.tsx#L72-L85) and fixtures are reused. The retry test adds a manually resolved promise and React Testing Library’s `act` helper to control when the pending response completes. No build configuration or shared test infrastructure changes are included.

## Evidence

The PR description reports:

- The author ran `bazel test --build_tests_only //frontend/...`; result: `//frontend:jest_test` **PASSED**.
- The author ran `cd frontend && npm run build`; result: TypeScript compilation and Vite build.
- The author ran `./tools/checks run`; result: no findings.
- The author added Jest coverage for the classifier table, failure-card copy and retry callback, page responses for 404/410/503/network failures, retry issuing two view requests before rendering results, and the event-404 permanent state without retry.
- The author exercised the results recipient page live on Vite against a local fake API; result: 404 displayed permanent copy without a button, and 503 displayed transient copy with **Try again**. The author reported that the plan share page could not be exercised on this branch because `SharePlanPage` was not present.

No screenshots are registered.
