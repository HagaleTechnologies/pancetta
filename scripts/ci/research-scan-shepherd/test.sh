#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
for fixture in fixtures/*.json; do
  [[ $fixture == *.expected.json ]] && continue
  jq -S -L . -f decide.jq "$fixture" > "$tmp/actual"
  jq -S . "${fixture%.json}.expected.json" > "$tmp/expected"
  diff -u "$tmp/expected" "$tmp/actual"
  echo "PASS: $(basename "$fixture" .json)"
done
logins=$(jq -cn -L . 'include "login"; [{__typename: "Bot", login: "catalyst-cloud-connector"},
  {__typename: "User", login: "catalyst-cloud-connector"}, null] | map(login)')
[[ $logins == '["catalyst-cloud-connector[bot]","catalyst-cloud-connector",null]' ]] || { echo "FAIL: login $logins" >&2; exit 1; }
echo "PASS: login"
