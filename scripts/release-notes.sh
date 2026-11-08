#!/bin/bash
# Print the CHANGELOG.md section for one version, to publish as its release
# notes. Fails if the changelog has no section for that version or it is empty,
# so a release can't go out without notes.
#
#   scripts/release-notes.sh 1.0.10 [CHANGELOG.md]
set -euo pipefail

version=${1:?usage: release-notes.sh VERSION [CHANGELOG]}
file=${2:-$(dirname "$0")/../CHANGELOG.md}
[ -r "$file" ] || { echo "Cannot read $file" >&2; exit 1; }

# The lines between "## <version>" and the next "## " heading, without the
# blank lines at either end.
notes=$(awk -v v="$version" '
    /^## / { if (found) exit; found = ($2 == v); next }
    found
' "$file" | sed -e '/./,$!d' | sed -e ':a' -e '/^\n*$/{$d;N;ba' -e '}')

if [ -z "$notes" ]; then
    echo "CHANGELOG.md has no notes for $version: add a \"## $version\" section first." >&2
    exit 1
fi
printf '%s\n' "$notes"
