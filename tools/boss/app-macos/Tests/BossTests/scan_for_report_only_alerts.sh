#!/bin/bash
# Generates GeneratedReportOnlyAlertScanResult.swift by scanning the app's
# production Swift sources for *report-only* modal alerts: a modal that just
# tells the user something (an error, a failure, a status) and offers no
# decision. Such a modal steals keyboard focus mid-typing and blocks the
# window for a message the user frequently cannot act on. The rule lives in
# tools/boss/app-macos/AGENTS.md ("No modals for non-actionable status or
# errors"); ReportOnlyAlertScanTests.swift asserts on this scan's output.
#
# What counts as report-only (and therefore an offender):
#
#   * A SwiftUI `.alert(` whose argument list carries `isPresented:` and
#     whose whole modifier — parenthesised arguments plus every trailing
#     closure (`actions:` / `message:` / bare) — contains fewer than two
#     `Button(` calls. A single OK/Cancel button is a dismissal, not a
#     decision. Static helpers that merely happen to be named `alert(`
#     (e.g. `QuitConfirmation.alert(agentCount:)`) carry no `isPresented:`
#     and are skipped.
#   * An AppKit `NSAlert()` whose enclosing `{ ... }` block calls
#     `addButton(withTitle:` fewer than two times. `NSAlert` with no added
#     buttons presents a lone default "OK".
#
#   * Any `.alert(` whose span mentions `workErrorMessage`, regardless of
#     button count: that property is the app-wide failure report and is
#     rendered by the passive `WorkNoticeBanner`, never a dialog.
#
# Confirmations (two or more buttons) and `.confirmationDialog(` are never
# flagged, so a legitimate decision dialog cannot trip this scan. The kept
# `.alert(` decisions are listed separately (`decisionAlerts`) so the test
# can assert that a confirmation dialog still presents.
#
# Like scan_for_real_openers.sh, this runs as a Bazel action against the
# real `srcs` Bazel resolved, because `bazel test` rewrites `#filePath` and
# a runtime filesystem scan would silently find nothing.
#
# Usage: scan_for_report_only_alerts.sh <swift-file>...
set -euo pipefail

offenders=()
decisions=()
for f in "$@"; do
  base="$(basename "$f")"
  while IFS= read -r finding; do
    case "$finding" in
      "KEEP "*) decisions+=("$base:${finding#KEEP }") ;;
      ?*) offenders+=("$base:$finding") ;;
    esac
  done < <(awk '
    # Collapse the file into one string, dropping whole-line comments so
    # a commented-out `.alert(` or `Button(` cannot skew the counts, and
    # remembering where each line starts so findings can cite a line.
    {
      line = $0
      sub(/^[ \t]+/, "", line)
      if (line ~ /^\/\//) { line = "" } else { line = $0 }
      starts[NR] = length(text) + 1
      text = text line "\n"
    }
    function line_of(pos,    n) {
      for (n = NR; n >= 1; n--) if (starts[n] <= pos) return n
      return 1
    }
    # Advance from `pos` (which must point at an opening bracket) to the
    # index just past its matching close, treating ( [ { uniformly and
    # skipping string literals.
    function skip_balanced(pos,    depth, c, len) {
      depth = 0; len = length(text)
      while (pos <= len) {
        c = substr(text, pos, 1)
        if (c == "\"") {
          pos++
          while (pos <= len && substr(text, pos, 1) != "\"") {
            if (substr(text, pos, 1) == "\\") pos++
            pos++
          }
        } else if (c == "(" || c == "[" || c == "{") {
          depth++
        } else if (c == ")" || c == "]" || c == "}") {
          depth--
          if (depth == 0) return pos + 1
        }
        pos++
      }
      return pos
    }
    # After the argument list, SwiftUI modifiers continue with trailing
    # closures, optionally labelled (`} message: {`). Consume them all.
    function skip_trailing_closures(pos,    rest, m) {
      while (1) {
        rest = substr(text, pos)
        if (match(rest, /^[ \t\n]*([A-Za-z_][A-Za-z0-9_]*:[ \t\n]*)?\{/)) {
          pos = skip_balanced(pos + RLENGTH - 1)
        } else {
          return pos
        }
      }
    }
    function count_of(hay, needle,    n, i) {
      n = 0
      while ((i = index(hay, needle)) > 0) { n++; hay = substr(hay, i + length(needle)) }
      return n
    }
    END {
      # SwiftUI `.alert(` modifiers.
      pos = 1
      while ((i = index(substr(text, pos), ".alert(")) > 0) {
        start = pos + i - 1
        open = start + length(".alert(") - 1
        end = skip_balanced(open)
        end = skip_trailing_closures(end)
        span = substr(text, start, end - start)
        pos = open + 1
        if (index(span, "isPresented:") == 0) continue
        if (index(span, "workErrorMessage") > 0) {
          printf "%d SwiftUI .alert( driven by `workErrorMessage` — that failure report is rendered by the passive `WorkNoticeBanner` in ContentView.chromeBanners, never a modal (see tools/boss/app-macos/AGENTS.md).\n", line_of(start)
          continue
        }
        buttons = count_of(span, "Button(")
        if (buttons >= 2) {
          printf "KEEP %d SwiftUI .alert( with %d buttons (decision)\n", line_of(start), buttons
        }
        if (buttons < 2) {
          printf "%d SwiftUI .alert( with %d button(s) is a report-only modal — show the message inline, on a chrome banner (`WorkNoticeBanner` / `ChatViewModel.workErrorMessage`), or just log it; only a decision with two or more choices may be modal (see tools/boss/app-macos/AGENTS.md).\n", line_of(start), buttons
        }
      }
      # AppKit NSAlert() instances.
      pos = 1
      while ((i = index(substr(text, pos), "NSAlert()")) > 0) {
        start = pos + i - 1
        pos = start + length("NSAlert()")
        # Walk forward to the close of the enclosing block.
        depth = 0; p = pos; len = length(text); blockEnd = len
        while (p <= len) {
          c = substr(text, p, 1)
          if (c == "\"") {
            p++
            while (p <= len && substr(text, p, 1) != "\"") {
              if (substr(text, p, 1) == "\\") p++
              p++
            }
          } else if (c == "{") {
            depth++
          } else if (c == "}") {
            if (depth == 0) { blockEnd = p; break }
            depth--
          }
          p++
        }
        span = substr(text, start, blockEnd - start)
        buttons = count_of(span, "addButton(withTitle:")
        if (buttons >= 2) {
          printf "KEEP %d NSAlert() with %d buttons (decision)\n", line_of(start), buttons
        }
        if (buttons < 2) {
          printf "%d NSAlert() adding %d button(s) is a report-only modal — show the message inline, on a chrome banner, or just log it; only a decision with two or more choices may be modal (see tools/boss/app-macos/AGENTS.md).\n", line_of(start), buttons
        }
      }
    }
  ' "$f")
done

echo "// Generated by scan_for_report_only_alerts.sh from the real Bazel srcs list."
echo "// Do not edit by hand — see ReportOnlyAlertScanTests.swift."
echo "enum GeneratedReportOnlyAlertScanResult {"
echo "    static let offenders: [String] = ["
for o in ${offenders[@]+"${offenders[@]}"}; do
  escaped="${o//\\/\\\\}"
  escaped="${escaped//\"/\\\"}"
  echo "        \"$escaped\","
done
echo "    ]"
echo "    static let decisionAlerts: [String] = ["
for d in ${decisions[@]+"${decisions[@]}"}; do
  escaped="${d//\\/\\\\}"
  escaped="${escaped//\"/\\\"}"
  echo "        \"$escaped\","
done
echo "    ]"
echo "}"
