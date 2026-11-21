#!/usr/bin/env bash
# Copyright 2026 Manu P Anand
# Licensed under the Apache License, Version 2.0
# See LICENSE for details.
#
# Check (default) or add (--fix) the Fornax license header on source files.
#
#   scripts/license-headers.sh          # exit 1 if any file lacks the header
#   scripts/license-headers.sh --fix    # prepend the header where missing
set -euo pipefail

cd "$(dirname "$0")/.."

YEAR_LINE="Copyright 2026 Manu P Anand"
L2="Licensed under the Apache License, Version 2.0"
L3="See LICENSE for details."
mode="${1:-check}"

header_for() {
  case "$1" in
    *.rs | *.ts | *.js)
      printf '// %s\n// %s\n// %s\n' "$YEAR_LINE" "$L2" "$L3" ;;
    *.sql)
      printf -- '-- %s\n-- %s\n-- %s\n' "$YEAR_LINE" "$L2" "$L3" ;;
    *.css)
      printf '/* %s\n   %s\n   %s */\n' "$YEAR_LINE" "$L2" "$L3" ;;
    *.svelte | *.html)
      printf '<!-- %s\n     %s\n     %s -->\n' "$YEAR_LINE" "$L2" "$L3" ;;
    *.sh | Dockerfile | compose.yaml)
      printf '# %s\n# %s\n# %s\n' "$YEAR_LINE" "$L2" "$L3" ;;
  esac
}

missing=0
while IFS= read -r -d '' file; do
  header="$(header_for "$(basename "$file")")"
  [ -z "$header" ] && continue
  if head -n 3 "$file" | grep -qF "$YEAR_LINE"; then
    continue
  fi
  if [ "$mode" = "--fix" ]; then
    tmp="$(mktemp)"
    first="$(head -n 1 "$file")"
    if [[ "$first" == '#!'* ]]; then
      { echo "$first"; echo "$header"; tail -n +2 "$file"; } >"$tmp"
    else
      { echo "$header"; echo; cat "$file"; } >"$tmp"
    fi
    cat "$tmp" >"$file"
    rm -f "$tmp"
    echo "added header: $file"
  else
    echo "missing header: $file"
    missing=1
  fi
done < <(find . \( -path ./target -o -path ./.git -o -path ./data -o -path ./web/node_modules -o -path ./web/dist \) -prune -o \
  -type f \( -name '*.rs' -o -name '*.sql' -o -name '*.sh' -o -name 'Dockerfile' -o -name 'compose.yaml' \
  -o -name '*.ts' -o -name '*.svelte' -o -name '*.css' -o -name '*.html' \) -print0)

exit "$missing"
