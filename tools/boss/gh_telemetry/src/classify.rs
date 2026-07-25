//! Pure classification of a `gh` argv, and pure extraction of the
//! `rateLimit` block from a GraphQL response.
//!
//! Everything here is a total function over data the caller already
//! has, so the whole classification layer is unit-testable without
//! spawning `gh` or reaching GitHub.

use crate::sample::{GhApi, RateLimit};

/// The GraphQL selection every engine query should fold in at the top
/// level so its response doubles as a quota reading.
///
/// `rateLimit` is charged 0 points and does not count as a call, so
/// adding it to a query is genuinely free. `cost` is the field that
/// makes per-caller attribution possible at all — `remaining` alone
/// only tells you the budget moved, not who moved it, which is exactly
/// the ambiguity that forced the original drain to be reconstructed
/// from reset-to-low-water crossings instead of measured.
pub const RATE_LIMIT_SELECTION: &str = "rateLimit { cost limit remaining used resetAt }";

/// What an argv says about the call it is about to make.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GhCallShape {
    pub api: GhApi,
    pub verb: String,
    pub endpoint: String,
}

/// Classify a `gh` argv into (API bucket, verb, endpoint).
///
/// Three shapes matter:
/// - `gh api graphql …` → GraphQL, endpoint `graphql`.
/// - `gh api <path> …` → REST, endpoint = normalised `<path>`; the verb
///   picks up an explicit `-X <METHOD>` when present, else `GET`.
/// - `gh <noun> <subcommand> …` → CLI porcelain; verb `<noun> <subcommand>`,
///   endpoint = the normalised first non-flag operand (usually a PR URL).
///
/// An empty or unrecognisable argv degrades to `Cli` with an `unknown`
/// verb rather than being dropped — an uncounted call is worse than a
/// vaguely-labelled one, since the whole point is that nothing escapes
/// the count.
pub fn classify_args(args: &[&str]) -> GhCallShape {
    let Some(&first) = args.first() else {
        return GhCallShape {
            api: GhApi::Cli,
            verb: "unknown".to_owned(),
            endpoint: String::new(),
        };
    };

    if first == "api" {
        let operands: Vec<&str> = api_operands(args);
        let path = operands.first().copied().unwrap_or("");
        if path == "graphql" {
            return GhCallShape {
                api: GhApi::Graphql,
                verb: "api graphql".to_owned(),
                endpoint: "graphql".to_owned(),
            };
        }
        // `gh api` defaults to POST, not GET, as soon as any field flag is
        // present — an explicit `-X`/`--method` still wins when given.
        let method = explicit_method(args).unwrap_or_else(|| {
            if args
                .iter()
                .any(|a| matches!(*a, "-f" | "-F" | "--field" | "--raw-field" | "--input"))
            {
                "POST"
            } else {
                "GET"
            }
        });
        return GhCallShape {
            api: GhApi::Rest,
            verb: format!("api {}", method.to_ascii_uppercase()),
            endpoint: normalize_endpoint(path),
        };
    }

    // Porcelain: `gh pr view <url> --json …`, `gh pr merge --auto …`.
    let subcommand = args.get(1).copied().filter(|s| !s.starts_with('-'));
    let verb = match subcommand {
        Some(sub) => format!("{first} {sub}"),
        None => first.to_owned(),
    };
    let endpoint = args
        .iter()
        .skip(2)
        .find(|a| !a.starts_with('-'))
        .map(|a| normalize_endpoint(a))
        .unwrap_or_default();
    GhCallShape {
        api: GhApi::Cli,
        verb,
        endpoint,
    }
}

/// Positional operands of a `gh api` invocation — everything that is
/// not a flag and not a flag's value.
///
/// `gh api` flags that take a value (`-X`, `-f`, `-F`, `-H`, `--jq`,
/// `--input`, …) must have that value skipped, or a `-f query={…}`
/// payload would be mistaken for the API path.
fn api_operands<'a>(args: &'a [&'a str]) -> Vec<&'a str> {
    const VALUE_FLAGS: &[&str] = &[
        "-X",
        "--method",
        "-f",
        "--raw-field",
        "-F",
        "--field",
        "-H",
        "--header",
        "--jq",
        "-q",
        "--input",
        "--template",
        "-t",
        "--hostname",
        "--cache",
    ];
    let mut out = Vec::new();
    let mut idx = 1; // skip the leading "api"
    while idx < args.len() {
        let arg = args[idx];
        if VALUE_FLAGS.contains(&arg) {
            idx += 2;
            continue;
        }
        if arg.starts_with('-') {
            idx += 1;
            continue;
        }
        out.push(arg);
        idx += 1;
    }
    out
}

/// The HTTP method from an explicit `-X` / `--method` flag, if present.
fn explicit_method<'a>(args: &'a [&'a str]) -> Option<&'a str> {
    args.iter()
        .position(|a| *a == "-X" || *a == "--method")
        .and_then(|i| args.get(i + 1))
        .copied()
}

/// Collapse the high-cardinality parts of an endpoint so the persisted
/// rows group usefully.
///
/// Without this, every PR number and commit SHA becomes its own
/// endpoint string and the attribution table degenerates into one row
/// per call. Owner/repo is deliberately *kept* — which repo a sweep is
/// spending against is a real question the data should answer.
///
/// - A full PR URL becomes `<owner>/<repo>/pull/:n`.
/// - A 40-char hex SHA becomes `:sha`.
/// - A bare numeric path segment becomes `:n`.
pub fn normalize_endpoint(raw: &str) -> String {
    let trimmed = raw
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_start_matches("github.com/")
        .trim_start_matches("api.github.com/")
        .trim_start_matches('/');
    if trimmed.is_empty() {
        return String::new();
    }
    // Drop a query string: `repos/o/r/pulls?head=x&state=open` — the
    // parameters vary per call and the path is what identifies the
    // endpoint.
    let path = trimmed.split('?').next().unwrap_or(trimmed);
    path.split('/')
        .map(|segment| {
            if is_hex_sha(segment) {
                ":sha"
            } else if !segment.is_empty() && segment.chars().all(|c| c.is_ascii_digit()) {
                ":n"
            } else {
                segment
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// A 40-character lowercase-or-uppercase hex string — a git object id.
fn is_hex_sha(segment: &str) -> bool {
    segment.len() == 40 && segment.chars().all(|c| c.is_ascii_hexdigit())
}

/// Extract the `rateLimit` block from a parsed GraphQL response body.
///
/// Returns `None` when the query didn't ask for `rateLimit`, when the
/// response never got that far, or when `cost`/`remaining`/`limit` are
/// missing — a partial block is not a usable reading and inventing
/// zeros for it would corrupt the very time series this exists to
/// build.
pub fn parse_rate_limit(body: &serde_json::Value) -> Option<RateLimit> {
    let node = body.get("data")?.get("rateLimit")?;
    Some(RateLimit {
        cost: Some(node.get("cost")?.as_i64()?),
        remaining: node.get("remaining")?.as_i64()?,
        limit: node.get("limit")?.as_i64()?,
        reset_at_ms: node.get("resetAt").and_then(|v| v.as_str()).and_then(parse_rfc3339_ms),
    })
}

/// Build a [`RateLimit`] from GitHub's `x-ratelimit-*` response headers,
/// which every REST response carries.
///
/// `lookup` is a case-insensitive header getter — taking a closure keeps
/// this crate free of any HTTP-client dependency, so both the
/// `reqwest`-based direct-API paths can share one parser.
///
/// For [`GhApi::Rest`], `cost` is `Some(1)`: the REST bucket meters whole
/// requests rather than points, and `x-ratelimit-used` is a running
/// window total, not this call's share. Reporting 1 keeps "sum of cost
/// over a window" a correct per-caller REST attribution instead of a
/// meaningless sum of snapshots.
///
/// For [`GhApi::Graphql`] — a GraphQL-over-HTTP call, since these headers
/// come from a REST-style response envelope regardless of which bucket
/// the request charged — `cost` is `None`: a GraphQL mutation's true
/// point cost is not recoverable from `x-ratelimit-*` headers, and
/// reporting `1` would invent a number that corrupts the very points
/// series this instrumentation exists to build. `remaining`/`limit`/
/// `reset_at_ms` are still meaningful (the gauge), so the reading is not
/// dropped entirely — see [`crate::sample::GhCallSample::points`].
pub fn parse_rate_limit_headers<'a>(
    api: crate::sample::GhApi,
    lookup: impl Fn(&str) -> Option<&'a str>,
) -> Option<RateLimit> {
    let remaining: i64 = lookup("x-ratelimit-remaining")?.trim().parse().ok()?;
    let limit: i64 = lookup("x-ratelimit-limit")?.trim().parse().ok()?;
    let reset_at_ms = lookup("x-ratelimit-reset")
        .and_then(|v| v.trim().parse::<i64>().ok())
        .map(|secs| secs * 1000);
    let cost = match api {
        crate::sample::GhApi::Graphql => None,
        crate::sample::GhApi::Rest | crate::sample::GhApi::Cli => Some(1),
    };
    Some(RateLimit {
        cost,
        remaining,
        limit,
        reset_at_ms,
    })
}

/// Parse GitHub's `resetAt` (RFC 3339 UTC, e.g. `2026-07-24T17:00:00Z`)
/// into epoch milliseconds.
///
/// Hand-rolled rather than pulling in a date crate: the field's shape is
/// fixed by the GraphQL schema, and this crate's whole value is being
/// cheap enough to sit on every egress path. Returns `None` on anything
/// that doesn't match, so a format change degrades to "no reset time"
/// rather than a bogus timestamp.
fn parse_rfc3339_ms(raw: &str) -> Option<i64> {
    let bytes = raw.as_bytes();
    if bytes.len() < 20 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    let year: i64 = raw.get(0..4)?.parse().ok()?;
    let month: i64 = raw.get(5..7)?.parse().ok()?;
    let day: i64 = raw.get(8..10)?.parse().ok()?;
    let hour: i64 = raw.get(11..13)?.parse().ok()?;
    let minute: i64 = raw.get(14..16)?.parse().ok()?;
    let second: i64 = raw.get(17..19)?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some((days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second) * 1000)
}

/// Days since the Unix epoch for a proleptic-Gregorian civil date.
/// Howard Hinnant's `days_from_civil` algorithm.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Whether a `gh` failure (or a GraphQL `errors` array) says the call
/// was rejected for exceeding the rate limit, as opposed to failing for
/// a network or query reason.
///
/// Both GitHub spellings are covered: the CLI's stderr text and the
/// GraphQL `RATE_LIMITED` error type that arrives alongside HTTP 200.
pub fn is_rate_limited(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("rate limit") || lower.contains("rate_limited")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_graphql_call() {
        let shape = classify_args(&["api", "graphql", "-f", "query={ rateLimit { remaining } }"]);
        assert_eq!(shape.api, GhApi::Graphql);
        assert_eq!(shape.verb, "api graphql");
        assert_eq!(shape.endpoint, "graphql");
    }

    #[test]
    fn graphql_query_payload_is_not_mistaken_for_the_path() {
        // `-f query={…}` must be skipped as a flag value. If it weren't,
        // the endpoint would be the whole query string — unbounded
        // cardinality and a GraphQL call misfiled as REST.
        let shape = classify_args(&["api", "-f", "query={ viewer { login } }", "graphql"]);
        assert_eq!(shape.api, GhApi::Graphql);
    }

    #[test]
    fn classifies_rest_get() {
        let shape = classify_args(&["api", "repos/spinyfin/mono/pulls/42"]);
        assert_eq!(shape.api, GhApi::Rest);
        assert_eq!(shape.verb, "api GET");
        assert_eq!(shape.endpoint, "repos/spinyfin/mono/pulls/:n");
    }

    #[test]
    fn classifies_rest_with_explicit_method() {
        let shape = classify_args(&["api", "-X", "PATCH", "repos/o/r/issues/7", "-f", "state=closed"]);
        assert_eq!(shape.api, GhApi::Rest);
        assert_eq!(shape.verb, "api PATCH");
        assert_eq!(shape.endpoint, "repos/o/r/issues/:n");
    }

    #[test]
    fn field_flag_without_explicit_method_defaults_to_post() {
        // `gh api` sends POST as soon as any field flag is given, even
        // with no `-X`/`--method` — defaulting to GET here would silently
        // mislabel the verb.
        let shape = classify_args(&["api", "repos/o/r/issues/7/comments", "-f", "body=hi"]);
        assert_eq!(shape.verb, "api POST");
    }

    #[test]
    fn explicit_method_wins_over_the_field_flag_default() {
        let shape = classify_args(&["api", "-X", "PATCH", "repos/o/r/issues/7", "-f", "state=closed"]);
        assert_eq!(shape.verb, "api PATCH");
    }

    #[test]
    fn no_flags_still_defaults_to_get() {
        let shape = classify_args(&["api", "repos/o/r/pulls/42"]);
        assert_eq!(shape.verb, "api GET");
    }

    #[test]
    fn classifies_porcelain_pr_view() {
        let shape = classify_args(&[
            "pr",
            "view",
            "https://github.com/spinyfin/mono/pull/2333",
            "--json",
            "state",
        ]);
        assert_eq!(shape.api, GhApi::Cli);
        assert_eq!(shape.verb, "pr view");
        assert_eq!(shape.endpoint, "spinyfin/mono/pull/:n");
    }

    #[test]
    fn classifies_porcelain_with_only_flags_after_subcommand() {
        let shape = classify_args(&["pr", "merge", "--auto", "--squash"]);
        assert_eq!(shape.verb, "pr merge");
        assert_eq!(shape.endpoint, "");
    }

    #[test]
    fn empty_argv_still_produces_a_countable_shape() {
        let shape = classify_args(&[]);
        assert_eq!(shape.api, GhApi::Cli);
        assert_eq!(shape.verb, "unknown");
    }

    #[test]
    fn normalizes_commit_sha_and_number_segments() {
        assert_eq!(
            normalize_endpoint("repos/o/r/commits/0123456789abcdef0123456789abcdef01234567/status"),
            "repos/o/r/commits/:sha/status",
        );
    }

    #[test]
    fn normalizes_query_string_away() {
        assert_eq!(
            normalize_endpoint("repos/o/r/pulls?head=o:branch&state=open"),
            "repos/o/r/pulls",
        );
    }

    #[test]
    fn parses_full_rate_limit_block() {
        let body = serde_json::json!({
            "data": { "rateLimit": {
                "cost": 3, "limit": 5000, "remaining": 4711, "used": 289,
                "resetAt": "2026-07-24T17:00:00Z",
            } }
        });
        let rl = parse_rate_limit(&body).expect("full block parses");
        assert_eq!(rl.cost, Some(3));
        assert_eq!(rl.remaining, 4711);
        assert_eq!(rl.limit, 5000);
        // 2026-07-24T17:00:00Z
        assert_eq!(rl.reset_at_ms, Some(1_784_912_400_000));
    }

    #[test]
    fn partial_rate_limit_block_is_rejected() {
        // `remaining` alone (what `build_batch_query` used to request) is
        // not a usable per-call reading — treat it as absent rather than
        // inventing a zero cost that would silently under-attribute.
        let body = serde_json::json!({ "data": { "rateLimit": { "remaining": 4711 } } });
        assert_eq!(parse_rate_limit(&body), None);
    }

    #[test]
    fn missing_reset_at_still_yields_a_reading() {
        let body = serde_json::json!({
            "data": { "rateLimit": { "cost": 1, "limit": 5000, "remaining": 10 } }
        });
        let rl = parse_rate_limit(&body).expect("reading without resetAt is still usable");
        assert_eq!(rl.reset_at_ms, None);
    }

    #[test]
    fn response_without_rate_limit_yields_none() {
        let body = serde_json::json!({ "data": { "repository": { "pullRequest": null } } });
        assert_eq!(parse_rate_limit(&body), None);
    }

    #[test]
    fn detects_both_rate_limit_spellings() {
        assert!(is_rate_limited("gh: API rate limit exceeded (HTTP 429)"));
        assert!(is_rate_limited(r#"[{"type":"RATE_LIMITED","message":"…"}]"#));
        assert!(!is_rate_limited("could not connect to host"));
    }

    #[test]
    fn parses_rate_limit_from_rest_headers() {
        let headers = [
            ("x-ratelimit-limit", "5000"),
            ("x-ratelimit-remaining", "4873"),
            ("x-ratelimit-used", "127"),
            ("x-ratelimit-reset", "1784912400"),
        ];
        let rl = parse_rate_limit_headers(GhApi::Rest, |name| {
            headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| *v)
        })
        .expect("REST headers parse");
        assert_eq!(rl.cost, Some(1), "REST meters whole requests, not points");
        assert_eq!(rl.remaining, 4873);
        assert_eq!(rl.limit, 5000);
        assert_eq!(rl.reset_at_ms, Some(1_784_912_400_000));
    }

    #[test]
    fn rest_headers_without_remaining_yield_none() {
        assert_eq!(parse_rate_limit_headers(GhApi::Rest, |_| None), None);
    }

    #[test]
    fn graphql_over_http_headers_yield_no_cost() {
        // A GraphQL mutation's true point cost is not recoverable from
        // REST-style rate-limit headers — reporting cost:1 here would
        // invent a number rather than leaving the undercount visible.
        let headers = [
            ("x-ratelimit-limit", "5000"),
            ("x-ratelimit-remaining", "4873"),
            ("x-ratelimit-reset", "1784912400"),
        ];
        let rl = parse_rate_limit_headers(GhApi::Graphql, |name| {
            headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| *v)
        })
        .expect("headers still parse");
        assert_eq!(rl.cost, None);
        assert_eq!(rl.remaining, 4873, "the gauge reading must still come through");
    }

    #[test]
    fn rejects_malformed_reset_at() {
        assert_eq!(parse_rfc3339_ms("not-a-timestamp"), None);
        assert_eq!(parse_rfc3339_ms("2026-13-01T00:00:00Z"), None);
    }
}
