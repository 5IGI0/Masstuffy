#!/usr/bin/env bash
# Prints the body of the `## [<version>]` section of CHANGELOG.md (without its heading).
# Exits 1 if the section is missing or empty. Used by .github/workflows/release-image.yml.
#
# usage: scripts/changelog-section.sh <version> [CHANGELOG.md]
set -euo pipefail

version=${1:?usage: $0 <version> [changelog]}
file=${2:-CHANGELOG.md}

body=$(awk -v v="$version" '
  /^## \[/ { if (found) exit; found = (index($0, "## [" v "]") == 1); next }
  /^\[[^]]+\]: / { if (found) exit }
  found { print }
' "$file" | sed -e '/./,$!d' | sed -e ':a' -e '/^\n*$/{$d;N;ba' -e '}')

if [[ -z "${body//[[:space:]]/}" ]]; then
  echo "no '## [$version]' section (or an empty one) in $file" >&2
  exit 1
fi
printf '%s\n' "$body"
