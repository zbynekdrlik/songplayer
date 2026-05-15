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
        # Record any test(#N) commit so a following fix(#N) can pair against
        # it. We match #N strictly inside the parens — `test(#94):` records
        # 94. `test: …` plain doesn't trigger; `test(refactor):` doesn't
        # either (no issue reference, nothing to pair).
        if [[ "$subject" =~ ^test\(#([0-9]+)\) ]]; then
            test_seen[${BASH_REMATCH[1]}]="$sha"
        fi
        # Only flag `fix(#N):` — a bug-fix commit explicitly tagged with an
        # issue number — as needing a paired test commit. `fix(ci):`,
        # `fix(scope):`, and `fix: …` plain are excluded because they may
        # not refer to an issue at all; mentions of `#N` in the rest of the
        # subject (e.g. "(PR #97)") are bundle references, not bug-fix
        # claims. The pre-push hook in airuleset is the broader,
        # label-aware enforcement; this CI gate covers the specific
        # `fix(#N)` convention SongPlayer uses.
        if [[ "$subject" =~ ^fix\(#([0-9]+)\) ]]; then
            issue="${BASH_REMATCH[1]}"
            if [ -z "${test_seen[$issue]:-}" ]; then
                violations+=("$sha #$issue $subject")
            fi
        fi
    done <<< "$log"

    if [ ${#violations[@]} -gt 0 ]; then
        echo "RED-GREEN order violation: each fix(#N) commit needs a preceding test(#N) commit in the same range."
        echo "See regression-test-first.md."
        echo ""
        echo "Offending commits:"
        for v in "${violations[@]}"; do
            echo "  $v"
        done
        return 1
    fi
    echo "ok: all fix(#N) commits in $range have a preceding test(#N) commit"
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
    )

    local good_rc bad_rc
    good_rc=0
    bad_rc=0
    ( cd "$tmp" && "$SCRIPT" fixture-base..fixture-good >/dev/null 2>&1 ) || good_rc=$?
    ( cd "$tmp" && "$SCRIPT" fixture-base..fixture-bad >/dev/null 2>&1 ) || bad_rc=$?

    if [ "$good_rc" -ne 0 ]; then
        echo "self-test FAIL: well-formed range expected rc=0, got $good_rc"
        return 1
    fi
    if [ "$bad_rc" -ne 1 ]; then
        echo "self-test FAIL: violation range expected rc=1, got $bad_rc"
        return 1
    fi
    echo "ok: self-test passed (well-formed → rc=0, violation → rc=1)"
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
