#!/usr/bin/env bash
# Batch 65: characterize every curated corpus + a sample raw day.
# Runs sequentially; expect ~90 min total wall-clock.
set -euo pipefail

cd "$(dirname "$0")/.."

EXAMPLE="cargo run --release -p pancetta-research --example batch65_corpus_characterize --"

# Curated manifests (relative to workspace root).
for m in \
    research/corpus/curated/ft8/hard_200.manifest.json \
    research/corpus/curated/ft8/hard_1000.manifest.json \
    research/corpus/curated/ft8/chrono_replay.manifest.json \
    research/corpus/curated/ft8/chrono_replay_mini33.manifest.json \
    research/corpus/curated/ft8/hard_jt9_rich_200.manifest.json \
    research/corpus/curated/ft8/lid_of_band.manifest.json \
    research/corpus/curated/ft8/wild_50.manifest.json \
    research/corpus/curated/ft8/wild_100.manifest.json
do
    echo "=== characterizing $m ==="
    $EXAMPLE --manifest "$m" \
        > "/tmp/batch65_$(basename "$m" .manifest.json).out" 2>&1 || true
done

# Sample from each raw recording day (--limit 500 caps wall-clock per day).
for day in 20260419 20260424 20260425 20260426 20260428 20260530; do
    echo "=== characterizing raw_$day (500-slot sample) ==="
    $EXAMPLE --dir /Users/thagale/.pancetta/recordings \
        --filter "ft8_${day}_" --limit 500 --name "raw_${day}_sample500" \
        > "/tmp/batch65_raw_${day}.out" 2>&1 || true
done

echo "=== done ==="
ls -1 research/corpus/characterizations/
