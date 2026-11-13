#!/usr/bin/env bash
# Exercise the actual Supply Chain fan-in without contacting GitHub.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
python3 - <<'PY'
from pathlib import Path
import os
import subprocess

workflow = Path('.github/workflows/supply-chain.yml').read_text()
job = workflow.split('\n  machete:\n', 1)[1].split('\n  # Fan-in gate.', 1)[0]
assert "if: needs.changes.outputs.deps == 'true'" in job
assert 'cargo install --locked cargo-machete --version 0.9.2' in job
assert '- run: cargo machete' in job
assert 'continue-on-error' not in job
assert 'pull_request:' in workflow and 'merge_group:' in workflow
fanin = workflow.split('\n  supply-chain:\n', 1)[1]
assert 'actions-pins, machete]' in fanin
assert 'MACHETE: ${{ needs.machete.result }}' in fanin
assert 'needs.changes.outputs.deps' in fanin
script = fanin.split('        run: |\n', 1)[1]
script = '\n'.join(line[10:] for line in script.splitlines())
for event in ['pull_request', 'merge_group']:
    for deps in ['true', 'false']:
        for result in ['success', 'failure', 'cancelled', 'skipped']:
            env = dict(os.environ, CHANGES='success', DEPS=deps, POLICY='success', PINS='success', MACHETE=result,
                       OSV_PR='success' if event == 'pull_request' else 'skipped',
                       OSV_MG='success' if event == 'merge_group' else 'skipped')
            process = subprocess.run(['bash', '-eu', '-c', script], env=env, capture_output=True, text=True)
            expected = deps != 'true' or result == 'success'
            assert (process.returncode == 0) == expected, (event, deps, result, process.stdout)
# Neither change detection nor action pin enforcement may silently skip.
for key in ['CHANGES', 'PINS', 'POLICY', 'OSV_PR']:
    for result in ['failure', 'cancelled', 'skipped']:
        env = dict(os.environ, CHANGES='success', DEPS='true', POLICY='success', PINS='success', MACHETE='success', OSV_PR='success', OSV_MG='skipped')
        env[key] = result
        assert subprocess.run(['bash', '-eu', '-c', script], env=env, capture_output=True).returncode != 0, (key, result)
print('Supply Chain machete wiring and fan-in cases passed')
PY
