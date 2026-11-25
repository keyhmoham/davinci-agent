#!/usr/bin/env bash
# Run every request in a file (default: examples/tasks.txt) against a running
# stack and print the mode, verification and token spend of each.
#
#   scripts/bench.sh                       # via docker compose
#   DFAE_BIN=./target/release/dfae DFAE_URL=http://127.0.0.1:8080 scripts/bench.sh
#
# Approvals are auto-approved, which requires DFAE_ALLOW_AUTO_APPROVE=true on the
# server. Use it for benchmarks only.
set -uo pipefail
cd "$(dirname "$0")/.."
file="${1:-examples/tasks.txt}"
run() {
  if [[ -n "${DFAE_BIN:-}" ]]; then "$DFAE_BIN" --ask "$1" --auto-approve
  else docker compose run --rm -T tui --ask "$1" --auto-approve; fi
}
printf '%-62s %s\n' "REQUEST" "RESULT"
printf '%-62s %s\n' "-------" "------"
while IFS= read -r goal; do
  [[ -z "$goal" || "$goal" == \#* ]] && continue
  summary=$(run "$goal" 2>&1 >/dev/null | grep -E '^\[(completed|failed|cancelled)\]' | tail -1)
  printf '%-62s %s\n' "${goal:0:60}" "${summary:-no result}"
done < "$file"
