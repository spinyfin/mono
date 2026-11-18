# PR #1545: Distinguishing temporary share-load failures from unavailable links

## 1. Problem being solved

The results recipient page previously treated every failed view request as evidence that the link was unavailable. Its [old error branch](https://github.com/brianduff/flunge/blob/b4e72c775d4e60b5fe0565599f666fdcd2a598cd/frontend/src/ShareResultsPage.tsx#L45-L51) sent both missing tokens and query errors to the [same card](https://github.com/brianduff/flunge/blob/b4e72c775d4e60b5fe0565599f666fdcd2a598cd/frontend/src/ShareResultsPage.tsx#L79-L85):

```tsx
if (!token || viewQuery.isError) {
  return <UnavailableState />;
}
```

Consequently, a temporary backend outage produced “This link isn't available.” with no retry action, just like an invalid link. The author identifies deploy-time connection failures, timeouts, and 502/503/504 responses as the motivating cases.

## 2. Core fix

The revision separates failure policy from presentation: `shareLoadFailure.ts` defines classification and copy, `ShareLoadFailureState` renders the card, and `ShareResultsPage` supplies a retry callback for transient failures.

The [classification and copy](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/shareLoadFailure.ts#L3-L44) establish this contract:

| Failure                                                     | Page behavior                                                                            |
| ----------------------------------------------------------- | ---------------------------------------------------------------------------------------- |
| `HttpError` with 400–499, except 408/429                    | “This link isn't available.” / “Check the link with whoever shared it.”; no retry button |
| 408/429, other `HttpError` statuses, or any non-`HttpError` | “Couldn't load this right now.” / “Try again in a moment.”; retry button                 |
| Missing URL token                                           | Permanent card; view query disabled                                                      |

This is a status-based presentation policy, not a check of the token's actual validity. Unknown errors default to transient. The integration is on the existing [`/r/:token` route](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/main.tsx#L182-L187). The author identifies reuse by the upcoming plan recipient page as follow-up work.

## 3. Implementation walkthrough and worked example

**Start with the existing error transport.** [`getShareResultsView`](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/api/v3/share.ts#L79-L91) calls `GET /api/v3/share/results/view` with the share-token header. The unchanged [HTTP helper](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/api/http.ts#L102-L115) already throws `HttpError` with the response status on a non-OK response. The revision consumes that existing information rather than changing the request API.

**Then read the classifier.** Its decisive logic is [small and ordered](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/shareLoadFailure.ts#L33-L44):

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

The 408/429 exception precedes the general 4xx branch. Only actual `HttpError` instances receive status classification; error-message text is not parsed.

**Next follow the page state transition.** The [view query](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/ShareResultsPage.tsx#L29-L32) keeps `enabled: !!token` and `retry: false`. The [new error branch](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/ShareResultsPage.tsx#L45-L64) handles a missing token separately, displays loading while an errored query is fetching again, and otherwise renders:

```tsx
const kind = classifyShareLoadFailure(viewQuery.error);
return (
  <ShareLoadFailureState
    kind={kind}
    onRetry={kind === "transient" ? () => void viewQuery.refetch() : undefined}
  />
);
```

The explicit `isFetching` check prevents the previous error card from persisting during recovery. The change adds a user-triggered refetch and no polling interval.

**Presentation remains reusable.** [`ShareLoadFailureState`](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/components/ShareLoadFailureState.tsx#L12-L36) preserves the centered layout and `LinkOff` icon, selects copy by kind, and uses the existing [`StateCard.action` slot](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/components/PageStates.tsx#L33-L65). The component displays a button whenever `onRetry` is supplied; the results page enforces the transient-only rule by withholding that callback for permanent failures.

**Worked example — the added 503 recovery test.** The [test scenario](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/ShareResultsPage.test.tsx#L163-L190) uses `/r/aK9mQx2LpN4rT8wB`. Its first view request rejects with `HttpError("Failed to GET", 503)`; its second request remains pending until the test resolves it with the existing [`VIEW` fixture](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/ShareResultsPage.test.tsx#L35-L49), whose fencer name is `DOE Jane`.

1. **Before:** the token exists, but `viewQuery.isError` is true. The old branch above renders the generic unavailable card; it supplies no retry action.
2. **After:** 503 bypasses both the 408/429 exception and the permanent 4xx range, so classification returns transient. The page displays the temporary-failure copy and **Try again**.
3. **On click:** `viewQuery.refetch()` reissues the request for the same token. While pending, the page shows “Loading results...” and removes the transient error copy.
4. **On success:** the returned fixture reaches the [normal results rendering](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/ShareResultsPage.tsx#L62-L74). The test observes “DOE Jane” and exactly two view calls, both with the original token.

The contrasting [404/410 tests](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/ShareResultsPage.test.tsx#L120-L141) retain permanent copy without a retry button.

Event failures remain a separate path. The unchanged [event-query handler](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/ShareResultsPage.tsx#L116-L135) invalidates the parent view query only for an event `HttpError(404)`. That does not itself classify the entire page as permanent: the [updated regression test](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/ShareResultsPage.test.tsx#L263-L276) also makes the subsequent view request return 404.

## 4. Tests and test infrastructure

- **Added classifier tests:** the [status table](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/shareLoadFailure.test.ts#L4-L25) asserts permanent results for 400/403/404/410 and transient results for 408/429/500/502/503/504. A separate test checks `TypeError`, ordinary `Error`, a string, and `undefined` as transient.
- **Added component tests:** [two cases](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/components/ShareLoadFailureState.test.tsx#L5-L22) assert both copy variants, no retry for the permanent rendering, and one callback invocation when the transient button is clicked.
- **Added page tests:** [410, network failure, and 503 cases](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/ShareResultsPage.test.tsx#L134-L161) check the corresponding copy and button presence. The recovery test above checks loading, error removal, successful rendering, and the exact two calls.
- **Modified tests:** the former generic-error case now uses [`HttpError(404)`](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/ShareResultsPage.test.tsx#L120-L132), adding guidance and no-retry assertions while preserving the absence of account prompts and revoked/expired wording. The event-404 regression adds the no-retry assertion. No test is deleted outright.
- **Infrastructure:** the page suite adds [`act`](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/ShareResultsPage.test.tsx#L1) and a manually resolved promise for the retry test. It reuses the existing fixtures and [QueryClient/MemoryRouter helper](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/ShareResultsPage.test.tsx#L72-L85). No build or test-runner configuration changes are included.

## Evidence

The PR description reports:

- The author ran `bazel test --build_tests_only //frontend/...`; result: `//frontend:jest_test` **PASSED**.
- The author ran `cd frontend && npm run build`; result: TypeScript compilation and Vite build.
- The author ran `./tools/checks run`; result: no findings.
- The revision added classifier-table, card-copy/button, and page recovery tests described above. The author reports that **Try again** reissues `getShareResultsView` for two calls total and then renders the view, and that the event-404 scenario retains permanent copy with no retry.
- The author exercised `/r/:token` live in Vite against a local fake API; result: 404 displayed permanent copy without a button, while 503 displayed transient copy with **Try again**. The author reports that the plan share page could not be exercised because `SharePlanPage` was not present on this branch.

No screenshots are registered for this task or its revisions.
