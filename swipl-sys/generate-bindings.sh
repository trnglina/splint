#!/usr/bin/env bash

set -euo pipefail

readonly expected_plversion=100002
readonly swipl_command="${SWIPL:-swipl}"
readonly crate_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly bindings_file="$crate_dir/src/bindings.rs"

runtime_variables="$("$swipl_command" --dump-runtime-variables=sh)"
plbase="$(sed -n 's/^PLBASE="\(.*\)";$/\1/p' <<<"$runtime_variables")"
plversion="$(sed -n 's/^PLVERSION="\(.*\)";$/\1/p' <<<"$runtime_variables")"

if [[ -z "$plbase" || -z "$plversion" ]]; then
  echo "failed to read PLBASE and PLVERSION from SWI-Prolog" >&2
  exit 1
fi

if [[ "$plversion" != "$expected_plversion" ]]; then
  echo "expected SWI-Prolog PLVERSION $expected_plversion, found $plversion" >&2
  exit 1
fi

readonly header="$plbase/include/SWI-Prolog.h"
generated="$(mktemp -t splint-bindings.XXXXXX)"
trap 'rm -f "$generated"' EXIT

bindgen \
  --allowlist-file "$header" \
  --blocklist-type '^wchar_t$' \
  --blocklist-type '^__darwin_wchar_t$' \
  --no-doc-comments \
  --no-layout-tests \
  --output "$generated" \
  --raw-line 'pub type wchar_t = ::std::os::raw::c_int;' \
  "$header" \
  -- \
  "-I$plbase/include"

case "${1:---write}" in
  --write)
    cp "$generated" "$bindings_file"
    ;;
  --check)
    if ! cmp -s "$generated" "$bindings_file"; then
      diff -u "$bindings_file" "$generated" || true
      echo "SWI-Prolog bindings are stale; run $0 --write" >&2
      exit 1
    fi
    ;;
  *)
    echo "usage: $0 [--write|--check]" >&2
    exit 2
    ;;
esac
