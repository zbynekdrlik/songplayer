#!/usr/bin/env bash
# check-red-green-order.sh — enforce regression-test-first.md commit ordering.
#
# For every commit in <range> whose subject matches `fix(#<N>)` (a bug-fix
# commit referencing GitHub issue N), require that an earlier commit in the
# same range has subject matching `test(#<N>)`. This catches the failure
# mode #92 was filed for: bug-fix commits landing without a paired RED
# regression test that proves the bug existed.
#
# Usage:
#   scripts/check-red-green-order.sh <git-range>
#   scripts/check-red-green-order.sh --self-test
#
# Exit codes:
#   0 — every fix(#N) commit has a preceding test(#N) commit (or no fix
#       commits in the range at all)
#   1 — at least one fix(#N) commit is missing its RED test commit
#   2 — usage / internal error
#
# Notes:
#   * Skips commits that don't follow the `fix(#N)` / `test(#N)` convention.
#   * Multiple `fix(#N)` commits referencing the same N are all covered by
#     a single preceding `test(#N)` commit (the test is the regression
#     guard, the fix can land in multiple commits if needed).
#   * A `[no-test: <reason>]` marker anywhere in a fix commit's full body is
#     a LOGGED bypass (per regression-test-first.md) — not a violation.
#   * Any `test(<scope>)` commit whose subject mentions `(#N)` anywhere (e.g.
#     `test(config): RED — … (#145)`) counts as the RED commit for N, not
#     only the strict `test(#N)` leading form.

set -euo pipefail

# Resolve $0 to an absolute path BEFORE any subshell `cd` happens. The
# self-test sub-process invokes "$SCRIPT" from a tmp working directory,
# so a relative $0 (e.g. `scripts/check-red-green-order.sh` as CI calls
# it) would fail with rc=127 (command not found).
SCRIPT="$(cd "$(dirname "$0")" && pwd)/$(basename "$0")"

check_range() {
    local range="$1"
    # `--reverse` makes the order chronological: ancestor first.
    local log
    log="$(git log --reverse --format='%H%x09%s' "$range" 2>/dev/null || true)"
    if [ -z "$log" ]; then
        echo "ok: no commits in range $range"
        return 0
    fi

    declare -A test_seen=()
    local violations=()

    while IFS=$'\t' read -r sha subject; do
        # Record any test commit that references an issue so a following
        # fix(#N) can pair against it. A `test(<scope>)` subject counts for
        # EVERY `#N` it mentions anywhere (e.g. `test(config): RED — … (#145)`
        # records 145), which subsumes the strict `test(#N)` leading form.
        # `test: …` plain / `test(refactor):` with no `#N` don't trigger.
        if [[ "$subject" =~ ^test\( ]]; then
            local rest="$subject"
            while [[ "$rest" =~ \#([0-9]+) ]]; do
                test_seen[${BASH_REMATCH[1]}]="$sha"
                rest="${rest#*"${BASH_REMATCH[0]}"}"
            done
        fi
        # Flag every bug-prefix subject that explicitly tags an issue —
        # `fix(#N):`, `bug(#N):`, `bugfix(#N):`, `hotfix(#N):`,
        # `regression(#N):`, `repair(#N):`, `patch(#N):` — as needing a
        # paired test commit. The prefix set matches the airuleset
        # pre-push hook in `regression-test-first.md`. Scope-only forms
        # like `fix(ci):` / `fix: …` plain remain excluded (no #N to
        # pair against); bundle references in trailers ("(PR #97)") are
        # not bug-fix claims.
        if [[ "$subject" =~ ^(fix|bug|bugfix|hotfix|regression|repair|patch)\(#([0-9]+)\) ]]; then
            issue="${BASH_REMATCH[2]}"
            # A `[no-test: <reason>]` marker anywhere in the full commit body
            # is a LOGGED bypass (regression-test-first.md), not a violation.
            local body
            body="$(git log -1 --format=%B "$sha")"
            if [[ "$body" =~ \[no-test:[^]]*\] ]]; then
                echo "bypass: $sha #$issue ${BASH_REMATCH[0]}"
                continue
            fi
            if [ -z "${test_seen[$issue]:-}" ]; then
                violations+=("$sha #$issue $subject")
            fi
        fi
    done <<< "$log"

    if [ ${#violations[@]} -gt 0 ]; then
        echo "RED-GREEN order violation: each fix(#N) commit needs a preceding test(#N) commit in the same range (or a [no-test: <reason>] body marker)."
        echo "See regression-test-first.md."
        echo ""
        echo "Offending commits:"
        for v in "${violations[@]}"; do
            echo "  $v"
        done
        return 1
    fi
    echo "ok: all fix(#N) commits in $range have a preceding test(#N) commit or a [no-test: …] bypass"
    return 0
}

self_test() {
    local tmp
    tmp="$(mktemp -d)"
    trap "rm -rf '$tmp'" RETURN
    (
        cd "$tmp"
        git init -q -b main
        git config user.email t@t
        git config user.name t

        # Fixture 1: well-formed — test commit before fix commit, same #N.
        git commit --allow-empty -q -m "initial"
        git tag fixture-base
        git commit --allow-empty -q -m "test(#42): add regression for foo bar"
        git commit --allow-empty -q -m "fix(#42): the actual fix"
        git tag fixture-good
        # Fixture 2: violation — fix commit with no preceding test commit.
        git commit --allow-empty -q -m "fix(#99): rushed fix without test"
        git tag fixture-bad

        # Fixture 3: well-formed across all bug-prefix subjects accepted by
        # the airuleset pre-push hook — every prefix has a paired test(#N).
        git commit --allow-empty -q -m "test(#201): regression for bug"
        git commit --allow-empty -q -m "bug(#201): the bug"
        git commit --allow-empty -q -m "test(#202): regression for bugfix"
        git commit --allow-empty -q -m "bugfix(#202): the bugfix"
        git commit --allow-empty -q -m "test(#203): regression for hotfix"
        git commit --allow-empty -q -m "hotfix(#203): the hotfix"
        git commit --allow-empty -q -m "test(#204): regression for regression"
        git commit --allow-empty -q -m "regression(#204): re-introduce check"
        git commit --allow-empty -q -m "test(#205): regression for repair"
        git commit --allow-empty -q -m "repair(#205): the repair"
        git commit --allow-empty -q -m "test(#206): regression for patch"
        git commit --allow-empty -q -m "patch(#206): the patch"
        git tag fixture-prefixes-good

        # Fixture 4: bug(#N) without preceding test(#N) — must fail.
        git commit --allow-empty -q -m "bug(#301): rushed bug fix without test"
        git tag fixture-bug-bad
        # Fixture 5: hotfix(#N) without preceding test(#N) — must fail.
        git commit --allow-empty -q -m "hotfix(#302): rushed hotfix without test"
        git tag fixture-hotfix-bad
        # Fixture 6: regression(#N) without preceding test(#N) — must fail.
        git commit --allow-empty -q -m "regression(#303): rushed regression without test"
        git tag fixture-regression-bad

        # Fixture 7: fix(#N) with a [no-test: …] body marker and NO test
        # commit — a LOGGED bypass, must pass.
        git commit --allow-empty -q -m "fix(#401): ops-only change" -m "[no-test: operational script, verified live]"
        git tag fixture-no-test-bypass

        # Fixture 8: test(<scope>) subject that mentions (#N) anywhere pairs a
        # following fix(#N) — must pass.
        git commit --allow-empty -q -m "test(config): RED — guard for foo (#402)"
        git commit --allow-empty -q -m "fix(#402): the fix"
        git tag fixture-scoped-test-good
    )

    local good_rc bad_rc prefixes_good_rc bug_bad_rc hotfix_bad_rc regression_bad_rc no_test_rc scoped_test_rc
    good_rc=0
    bad_rc=0
    prefixes_good_rc=0
    bug_bad_rc=0
    hotfix_bad_rc=0
    regression_bad_rc=0
    no_test_rc=0
    scoped_test_rc=0
    ( cd "$tmp" && "$SCRIPT" fixture-base..fixture-good >/dev/null 2>&1 ) || good_rc=$?
    ( cd "$tmp" && "$SCRIPT" fixture-base..fixture-bad >/dev/null 2>&1 ) || bad_rc=$?
    ( cd "$tmp" && "$SCRIPT" fixture-bad..fixture-prefixes-good >/dev/null 2>&1 ) || prefixes_good_rc=$?
    ( cd "$tmp" && "$SCRIPT" fixture-prefixes-good..fixture-bug-bad >/dev/null 2>&1 ) || bug_bad_rc=$?
    ( cd "$tmp" && "$SCRIPT" fixture-bug-bad..fixture-hotfix-bad >/dev/null 2>&1 ) || hotfix_bad_rc=$?
    ( cd "$tmp" && "$SCRIPT" fixture-hotfix-bad..fixture-regression-bad >/dev/null 2>&1 ) || regression_bad_rc=$?
    ( cd "$tmp" && "$SCRIPT" fixture-regression-bad..fixture-no-test-bypass >/dev/null 2>&1 ) || no_test_rc=$?
    ( cd "$tmp" && "$SCRIPT" fixture-no-test-bypass..fixture-scoped-test-good >/dev/null 2>&1 ) || scoped_test_rc=$?

    if [ "$good_rc" -ne 0 ]; then
        echo "self-test FAIL: well-formed range expected rc=0, got $good_rc"
        return 1
    fi
    if [ "$bad_rc" -ne 1 ]; then
        echo "self-test FAIL: violation range expected rc=1, got $bad_rc"
        return 1
    fi
    if [ "$prefixes_good_rc" -ne 0 ]; then
        echo "self-test FAIL: all-prefix paired range expected rc=0, got $prefixes_good_rc"
        return 1
    fi
    if [ "$bug_bad_rc" -ne 1 ]; then
        echo "self-test FAIL: bug(#N) without test expected rc=1, got $bug_bad_rc"
        return 1
    fi
    if [ "$hotfix_bad_rc" -ne 1 ]; then
        echo "self-test FAIL: hotfix(#N) without test expected rc=1, got $hotfix_bad_rc"
        return 1
    fi
    if [ "$regression_bad_rc" -ne 1 ]; then
        echo "self-test FAIL: regression(#N) without test expected rc=1, got $regression_bad_rc"
        return 1
    fi
    if [ "$no_test_rc" -ne 0 ]; then
        echo "self-test FAIL: fix(#N) with [no-test: …] marker expected rc=0, got $no_test_rc"
        return 1
    fi
    if [ "$scoped_test_rc" -ne 0 ]; then
        echo "self-test FAIL: test(<scope>) mentioning (#N) expected rc=0, got $scoped_test_rc"
        return 1
    fi
    echo "ok: self-test passed (all bug-prefix subjects gate correctly)"
    return 0
}

case "${1:-}" in
    "")
        echo "usage: $0 <git-range> | --self-test" >&2
        exit 2
        ;;
    --self-test)
        self_test
        ;;
    *)
        check_range "$1"
        ;;
esac
