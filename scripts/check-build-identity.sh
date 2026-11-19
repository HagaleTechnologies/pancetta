#!/usr/bin/env bash
# PAN-377: a release binary must name the commit and target it was built from.
set -euo pipefail
if [ "$#" -ne 2 ]; then echo '::error::usage: check-build-identity.sh BIN TARGET' >&2; exit 1; fi
bin=$1 target=$2
if ! output=$("$bin" --version; status=$?; printf '\037'; exit "$status"); then
  echo '::error::binary --version failed' >&2; exit 1
fi
output=${output%$'\037'}
if [[ $output != *$'\n' ]]; then echo '::error::version needs one newline' >&2; exit 1; fi
output=${output%$'\n'}
output=${output%$'\r'}
pattern='^pancetta [0-9]+\.[0-9]+\.[0-9]+[^ ]* \(([0-9a-f]{7,64}) ([0-9]{4}-[0-9]{2}-[0-9]{2}) ([^ )]+)\)$'
if [[ ! $output =~ $pattern ]]; then
  echo '::error::invalid --version line' >&2; exit 1
fi
sha=${BASH_REMATCH[1]} actual_target=${BASH_REMATCH[3]}
if [ "$actual_target" != "$target" ]; then echo "::error::target $actual_target differs from $target" >&2; exit 1; fi
if ! resolved=$(git rev-parse --verify --quiet "${sha}^{commit}") || [ "$resolved" != "$(git rev-parse HEAD)" ]; then
  echo "::error::commit $sha differs from HEAD" >&2; exit 1
fi
printf 'PASS: %s\n' "$output"
