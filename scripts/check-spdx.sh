#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
# Fails if any source file lacks the SPDX header in its first 3 lines.
set -u
cd "$(dirname "$0")/.."
dirs=()
for d in src migrations tests web/src web/scripts web/e2e; do
  [ -d "$d" ] && dirs+=("$d")
done
bad=0
if [ ${#dirs[@]} -gt 0 ]; then
  while IFS= read -r f; do
    if ! head -n 3 "$f" | grep -q 'SPDX-License-Identifier: AGPL-3.0-only'; then
      echo "missing SPDX header: $f"
      bad=1
    fi
  done < <(find "${dirs[@]}" -type f \( -name '*.rs' -o -name '*.sql' -o -name '*.ts' -o -name '*.tsx' -o -name '*.mjs' \) | sort)
fi
exit $bad
