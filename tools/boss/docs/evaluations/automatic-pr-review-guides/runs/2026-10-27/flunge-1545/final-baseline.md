**1. Problem: a failed request looked like a broken link**

Previously, the results recipient page (`/r/:token`) treated a missing token and every view-fetch failure identically:

```tsx
if (!token || viewQuery.isError) {
  return <UnavailableState />;
}
```

That card displayed “This link isn't available.” with no explanation or retry action. A valid link encountering a temporary outage therefore looked like an invalid or revoked link.

This PR changes the recipient-facing interpretation of failures. It does not change token validation or the API.

**2. Core fix: classify the failure, then offer an appropriate response**

The new [shareLoadFailure.ts](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/shareLoadFailure.ts#L19) defines the policy:

```ts
const TRANSIENT_CLIENT_STATUSES = new Set([408, 429]);

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

The ordering matters: 408 and 429 are exceptions to the general 4xx rule.

| Failure                                                    | Displayed response                                                      | Retry action  |
| ---------------------------------------------------------- | ----------------------------------------------------------------------- | ------------- |
| Missing URL token; `HttpError` with 4xx other than 408/429 | “This link isn't available.” / “Check the link with whoever shared it.” | None          |
| 408, 429, 5xx, network or unknown errors                   | “Couldn't load this right now.” / “Try again in a moment.”              | **Try again** |

Classification depends on the existing `HttpError` class, not error-message matching or an arbitrary object's `status` property. The existing [HTTP GET helper](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/api/http.ts#L102) already throws `HttpError` for unsuccessful HTTP responses and lets fetch failures propagate. No transport changes were necessary.

“Permanent” is the UI policy for these statuses; the classifier does not inspect the response body to determine whether a token specifically expired or was revoked.

**3. Logic and architecture: follow the classifier into the page**

The change separates three responsibilities:

- **Policy and copy:** `shareLoadFailure.ts` owns the failure kind and both messages.
- **Presentation:** [ShareLoadFailureState.tsx](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/components/ShareLoadFailureState.tsx#L12) replaces the page-local `UnavailableState`. It retains the `LinkOff` icon, centered layout and existing `StateCard`, using its existing `action` slot for the button.
- **Query orchestration:** [ShareResultsPage.tsx](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/ShareResultsPage.tsx#L45) decides which kind to display and supplies the retry callback.

The decisive page branch is:

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

The `isFetching` check makes loading take precedence when an error and an active fetch coexist. The ordinary loading branch below still handles initial loading. Missing tokens take a separate permanent-error path; the query remains disabled without a token.

The component itself shows a button whenever `onRetry` exists. **The page enforces the rule that only transient failures receive that callback.** This distinction matters for future consumers of the shared component.

The query retains `retry: false`, and no polling interval is introduced. Clicking the button refetches the existing query rather than reloading the browser.

**Worked example — the recovery test’s actual inputs**

The [new recovery test](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/ShareResultsPage.test.tsx#L163) starts at `/r/aK9mQx2LpN4rT8wB` with a fresh query client. The first `getShareResultsView` call rejects with `HttpError("Failed to GET", 503)`. A second call is configured to remain pending until the test resolves it with the `VIEW` fixture.

1. **Before:** the rejected query satisfies `viewQuery.isError`, immediately selecting `UnavailableState`. The recipient sees “This link isn't available.” and has no in-page retry action.
2. **After:** 503 is outside the permanent 4xx range, so classification returns `"transient"`. The recipient sees the temporary-failure message and **Try again**.
3. Clicking the button calls `viewQuery.refetch()`. While the second request is pending, the page shows “Loading results...” and removes the failure message.
4. Resolving that request with `VIEW` renders its fencer name, **DOE Jane**. The test verifies exactly two view calls, both with the same token.

The useful boundary is a 404 or 410: those produce permanent copy and no retry button.

An adjacent behavior remains intact: [event-level 404 handling](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/ShareResultsPage.tsx#L117) invalidates the overall view query. It does **not** directly declare the entire link unavailable. In the regression test, the resulting view refetch also returns 404, which produces the permanent page state. Other event failures continue to use “Unable to load this event.”

Only the results recipient page adopts the component here. The plan recipient page is absent from this revision; sharing this presentation with it remains follow-up work.

**4. Tests and supporting infrastructure**

The PR adds coverage at three levels:

- **[Classifier tests](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/shareLoadFailure.test.ts#L4):** table-driven cases for permanent 400/403/404/410 and transient 408/429/500/502/503/504. Additional assertions cover a fetch `TypeError`, ordinary `Error`, string and `undefined`.
- **[Component tests](https://github.com/brianduff/flunge/blob/ba2e4fce1b1de11fe605390ed84dfe9f0f415e99/frontend/src/components/ShareLoadFailureState.test.tsx#L5):** verify both messages, absence of a permanent retry action, and one callback invocation on a transient retry click.
- **Page tests:** replace the old generic-failure expectation with an explicit 404 case; add 410, network failure, 503 and successful retry coverage; strengthen the existing event-404 regression with a no-retry assertion.

There are **no test-runner, dependency or build-configuration changes**. Existing Jest/jsdom and Bazel test-file discovery include the new files. The page tests reuse their existing router, query client and API mocks. The only added testing mechanism is a manually resolved promise, completed inside React Testing Library’s `act`, so the retry test can observe the pending loading state before success.

I inspected the code and tests but did not execute them. The PR description reports passing frontend Bazel tests, TypeScript/Vite build and repository checks, plus manual fake-API checks for 404 and 503; no screenshots are registered in the supplied review context.

https://github.com/brianduff/flunge/pull/1545
