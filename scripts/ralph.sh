#!/usr/bin/env bash
# Usage (from repo root): bash scripts/ralph.sh
# Work happens in this checkout. Stop with Ctrl-C and run acceptance tests yourself.
set -euo pipefail

design="${RALPH_DESIGN:-docs/module-design.md}"

done_reply='<< DONE >>'
continue_reply='<< CONTINUE >>'
fail_reply='<< FAIL >>'
max_retries=3

implementation_prompt=$(cat <<EOF
Read the module design at $design and compare it with the current codebase.
Follow the rollout order in the design (SQLite first). Treat existing tests as
context, not as the implementation backlog or the completion criterion.

Pick exactly one smallest independently testable slice that closes a concrete
gap. Describe the expected behavior, module boundary, and what is out of scope.
Match the existing code and module boundaries. Do not select another slice or
declare the whole design complete after implementing the slice.

Only if you find no in-scope implementation gaps, reply with exactly:
$done_reply
EOF
)

cleanup_prompt=$(cat <<EOF
Review the codebase for one focused opportunity to refactor, simplify, or
remove unused code. Pick a change that makes the code easier to maintain and
preserves existing behavior and public interfaces. Avoid speculative
abstractions, broad rewrites, and unrelated formatting. Do not implement new
features or advance the design rollout in this iteration.

If you find no worthwhile, independently verifiable cleanup, reply with exactly:
$done_reply
EOF
)

workflow=$(cat <<EOF
Do the work yourself without delegating or starting another session.
Add focused unit tests where they add coverage, run the relevant unit tests,
and fix failures in your change. Leave acceptance tests unchanged and do not
run them. Preserve user changes.
Every iteration that makes a change must finish with one local commit.
After verification, use the commit skill to commit all and only your changes
from this iteration. Do not push. Report the changes, commit hash, checks run,
and any blocker. Run unattended: do not ask questions or run interactive commands.

Only after verifying the commit exists and none of your changes are left
uncommitted, end your reply with this line:
$continue_reply

If blocked, verification failed, or the commit failed, do not commit unverified
work. Explain why, report any uncommitted changes you made, and end with this line:
$fail_reply

Use those markers only for your final decision.
EOF
)

cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."

blue='' green='' red='' reset=''
if [[ -t 1 && -z "${NO_COLOR:-}" && "${TERM:-}" != dumb ]]; then
  blue=$'\033[34m' green=$'\033[32m' red=$'\033[31m' reset=$'\033[0m'
fi

status() {
  printf '%sRalph %d (%s) — %s%s\n' "$1" "$iteration" "$phase" "$2" "$reset"
}

log=$(mktemp "${TMPDIR:-/tmp}/ralph.XXXXXXXX")
trap 'rm -f -- "$log"' EXIT

for ((iteration = 1; ; iteration++)); do
  if ((iteration % 10 == 0)); then
    phase=cleanup
    prompt=$cleanup_prompt
  else
    phase=implementation
    prompt=$implementation_prompt
  fi

  status "$blue" 'Starting'
  for ((retry = 0; retry <= max_retries; retry++)); do
    start_commit=$(git rev-parse HEAD)
    if ! GIT_TERMINAL_PROMPT=0 NO_COLOR=1 opencode run --auto --agent engineer \
      "$prompt"$'\n\n'"$workflow" </dev/null 2>&1 | tee "$log"; then
      status "$red" 'Run failed'
      exit 1
    fi

    # Accept a marker at the end of the final nonblank line, even inline.
    result=$(awk '{ sub(/[[:space:]]+$/, ""); if (length) last = $0 } END { print last }' "$log")
    case "$result" in
      "$done_reply"|*[[:blank:]]"$done_reply") status "$green" 'Nothing to do; continuing' ;;
      "$continue_reply"|*[[:blank:]]"$continue_reply")
        if [[ "$(git rev-parse HEAD)" == "$start_commit" ]]; then
          status "$red" 'Reported success without a new commit'
          exit 1
        fi
        status "$green" 'Change committed'
        ;;
      "$fail_reply"|*[[:blank:]]"$fail_reply") status "$red" 'Change failed; continuing' ;;
      *)
        if ((retry == max_retries)); then
          status "$red" "No completion marker after $max_retries retries"
          exit 1
        fi
        status "$blue" "No completion marker; retrying ($((retry + 1))/$max_retries)"
        continue
        ;;
    esac
    break
  done
done
