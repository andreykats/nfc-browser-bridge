#!/bin/bash
# Print the staging Mac mini's latest daily summary from the logs bucket (TESTING.md). Run before
# dispatching a release. Needs a gcloud login with access to the logs-wvmye project.
#
#   scripts/staging-status.sh [HOST]      # HOST defaults to Mac-mini-F6E28B59
#
# Exits 1 if the latest summary says FAIL or is more than two days old, 2 if there is none.
set -euo pipefail

host=${1:-Mac-mini-F6E28B59}
bucket=gs://logs-wvmye.firebasestorage.app/nfc-browser-bridge/$host

latest=$(gcloud storage ls "$bucket/*/summary.*.txt" 2>/dev/null | sort -t/ -k7 | tail -1) || true
if [ -z "$latest" ]; then
    echo "No staging summary for $host in $bucket." >&2
    exit 2
fi
summary=$(gcloud storage cat "$latest")
echo "$latest"
echo "$summary"

day=$(basename "$latest" | sed -E 's/^summary\.([0-9-]+)\.txt$/\1/')
age=$(( ( $(date -u +%s) - $(date -u -j -f %F "$day" +%s) ) / 86400 ))
status=0
if [ "$age" -gt 2 ]; then
    echo "WARNING: the latest summary is $age days old; is the staging probe running?" >&2
    status=1
fi
if grep -q '^result: FAIL' <<<"$summary"; then
    status=1
fi
exit $status
