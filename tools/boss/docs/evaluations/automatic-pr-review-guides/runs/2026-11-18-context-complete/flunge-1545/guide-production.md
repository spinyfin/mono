# Share-link failures: distinguish unavailable links from temporary outages

## Orientation

Recipients opening shared fencing results could see “This link isn't available” during a temporary service outage, even when their link was valid. A deployment restart, connection failure, or temporary server error therefore looked like a broken link.

This bug fix changes the results-sharing page in Flunge's frontend. Failures classified as permanent keep the unavailable-link message and direct the recipient to the person who shared it. Temporary or unrecognized failures instead say “Couldn't load this right now” and offer **Try again**, which reloads the results within the page.

A **token** is the identifying value carried in the shared link. **Permanent** and **transient** are the page's failure categories: permanent offers no retry button; transient invites another attempt without promising that it will succeed.

## Change overview

The change replaces one generic failure card with two outcomes, using [centralized classification and copy](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/shareLoadFailure.ts#L3-L44):

| Failure                                                                          | Recipient sees                                                          | Action          |
| -------------------------------------------------------------------------------- | ----------------------------------------------------------------------- | --------------- |
| Missing URL token, or a recognized HTTP error with status 400–499 except 408/429 | “This link isn't available.” / “Check the link with whoever shared it.” | No retry button |
| Status 408/429, server errors, network failures, or other unrecognized errors    | “Couldn't load this right now.” / “Try again in a moment.”              | **Try again**   |

An **HTTP status** is the numeric outcome returned by the server; 408 means request timeout, 429 means too many requests, and 500–599 denote server failures.

The integration is the existing [public results route, `/r/:token`](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/main.tsx#L182-L187). The PR leaves owner-side sharing dialogs unchanged. Its description identifies reuse by the forthcoming plan-sharing page as later work; this PR supplies the reusable error presentation.

## Core implementation

The implementation separates three responsibilities:

- [`shareLoadFailure.ts`](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/shareLoadFailure.ts#L3-L44) defines the two failure categories, their wording, and the classification function.
- [`ShareLoadFailureState`](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/components/ShareLoadFailureState.tsx#L12-L36) displays the selected wording and an optional retry action, retaining the broken-link icon and centered layout.
- [`ShareResultsPage`](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/ShareResultsPage.tsx#L29-L64) classifies request failures and supplies the retry action only for transient failures.

The existing `StateCard` is the generic visual container for an icon, title, message, and optional action; its [action slot](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/components/PageStates.tsx#L33-L65) already supports the new button.

## Implementation walkthrough

### 1. Preserve the distinction between a server response and a failed connection

The unchanged [results request helper](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/api/v3/share.ts#L84-L91) requests `/api/v3/share/results/view`, passing the link token in a request header.

`HttpError` is the existing error type that retains a server response's numeric status. The [HTTP helper](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/api/http.ts#L102-L115) throws it for unsuccessful responses; a rejected network request propagates without that conversion.

The new [classifier](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/shareLoadFailure.ts#L19-L44) makes the retryable exceptions before applying the general client-error rule:

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

`TRANSIENT_CLIENT_STATUSES` contains 408 and 429. Classification uses the error type and status, not message text or a specific diagnosis of expiry or revocation. An arbitrary object with a `status` property is not a recognized `HttpError` and follows the transient fallback.

### 2. Let the page own retry policy and loading transitions

React Query is the library managing the request's data, loading state, and errors. Its `refetch()` method runs the request again. The [existing query configuration](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/ShareResultsPage.tsx#L29-L32) disables automatic failure retries and disables the request when no token exists. The change adds no polling interval.

The [new error branch](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/ShareResultsPage.tsx#L45-L64) handles a missing token separately as permanent. For a failed request, it first shows “Loading results...” if another fetch is underway, then classifies the settled error:

```tsx
const kind = classifyShareLoadFailure(viewQuery.error);
return (
  <ShareLoadFailureState
    kind={kind}
    onRetry={kind === "transient" ? () => void viewQuery.refetch() : undefined}
  />
);
```

The distinction between policy and presentation matters: the [card renders its button whenever `onRetry` is supplied](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/components/ShareLoadFailureState.tsx#L19-L32). The page, rather than the card's `kind` prop alone, enforces the no-retry-button policy for permanent failures.

### 3. Worked example: recover from a 503

The [retry test](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/ShareResultsPage.test.tsx#L163-L190) supplies a concrete sequence:

1. **Input and state:** the recipient opens `/r/aK9mQx2LpN4rT8wB`. The initial results request rejects with `HttpError("Failed to GET", 503)`; a second request is prepared to return the existing results fixture.
2. **Before:** the [old branch](https://github.com/brianduff/flunge/blob/b4e72c775d4e60b5fe0565599f666fdcd2a598cd/frontend/src/ShareResultsPage.tsx#L45-L51) sent every request error to the [same unavailable card](https://github.com/brianduff/flunge/blob/b4e72c775d4e60b5fe0565599f666fdcd2a598cd/frontend/src/ShareResultsPage.tsx#L79-L85), with no retry action.
3. **Now:** 503 falls through to transient. The recipient gets temporary-failure wording and clicks **Try again**. While the second request is pending, the loading message replaces the failure card.
4. **Observable result:** resolving that request displays “DOE Jane.” The test asserts exactly two results requests, both with the same token.

For contrast, the [404 and 410 cases](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/ShareResultsPage.test.tsx#L120-L141) show unavailable-link wording without a retry button.

### 4. Keep individual-event failures distinct from page failures

An event is a competition within a tournament, loaded separately from the results overview. The unchanged [event error handler](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/ShareResultsPage.tsx#L116-L135) normally leaves failures inside the event card. An event-level 404 invalidates the overview request—marks it for a fresh fetch—rather than directly declaring the entire link unavailable.

The [updated regression test](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/ShareResultsPage.test.tsx#L263-L277) makes that fresh overview request also return 404. That response produces the permanent page state and removes the previously visible fencer name.

## Tests and test infrastructure

- **Added classifier tests:** the [status table](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/shareLoadFailure.test.ts#L4-L26) asserts permanent for 400/403/404/410 and transient for 408/429/500/502/503/504. Network `TypeError`, generic `Error`, a string, and `undefined` all assert transient.
- **Added component tests:** [both copy variants](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/components/ShareLoadFailureState.test.tsx#L5-L23) are checked; permanent rendering without a callback has no button, and clicking the transient button calls its callback once.
- **Modified page tests:** the former generic-error test becomes an explicit [404 test](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/ShareResultsPage.test.tsx#L120-L132), retaining assertions against signup prompts and revoked/expired wording. The event-404 regression gains the no-retry assertion.
- **Added page tests:** [410, network failure, 503, and successful manual retry](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/ShareResultsPage.test.tsx#L134-L190) cover classification, loading feedback, and request arguments/count.

No standalone tests are removed. The existing [fixtures and routed query-client helper](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/ShareResultsPage.test.tsx#L35-L85) are reused. The retry test adds a manually resolved promise and React Testing Library's `act` helper to apply completion before assertions. No build or test-runner configuration changes are included.

## Evidence

The PR description reports:

- The author ran `bazel test --build_tests_only //frontend/...`; result: `//frontend:jest_test` **PASSED**.
- The author ran `cd frontend && npm run build`; result: TypeScript compilation and Vite build.
- The author ran `./tools/checks run`; result: **no findings**.
- The author added Jest coverage for classification, card wording and callback behavior, page failures, two-request retry recovery, and the event-404 permanent-state regression.
- The author exercised the results page live on Vite against a local fake API; result: 404 showed permanent wording with no button, while 503 showed transient wording with **Try again**. The author reported that the plan-sharing page could not be exercised because `SharePlanPage` was absent from the branch.

No screenshots are registered.
