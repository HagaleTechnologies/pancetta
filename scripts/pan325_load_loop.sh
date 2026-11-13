#!/usr/bin/env bash
# Reproduce PAN-325's timing checks while an independent build loads the host.
set -euo pipefail
cd "$(dirname "$0")/.."
iterations=20
while getopts 'n:' option; do
    case "$option" in n) iterations=$OPTARG ;; *) exit 2 ;; esac
done
[[ "$iterations" =~ ^[1-9][0-9]*$ ]] || { echo 'Expected a positive iteration count' >&2; exit 2; }
# Warm the test artifacts so compilation is not counted as test runtime.
cargo test -p pancetta-qso --lib pan72_stall_detection_tests --no-run
cargo test -p pancetta --features pancetta-ft8/transmit --lib pan71_playback_start_tests --no-run
python3 - "$iterations" <<'PYTHON'
import os, signal, statistics, subprocess, sys, tempfile, time
import atexit
iterations = int(sys.argv[1])
scratch = tempfile.TemporaryDirectory(prefix='pan325-load-')
build_log = open(os.path.join(scratch.name, 'build.log'), 'w+')
env = dict(os.environ, CARGO_TARGET_DIR=os.path.join(scratch.name, 'target'))
load = subprocess.Popen(['cargo', 'build', '--features', 'pancetta-ft8/transmit', '-p', 'pancetta'],
                        env=env, stdout=build_log, stderr=subprocess.STDOUT, start_new_session=True)
def cleanup():
    if load.poll() is None:
        os.killpg(load.pid, signal.SIGTERM)
    load.wait()
    build_log.close()
    scratch.cleanup()
atexit.register(cleanup)
commands = [
    ['cargo', 'test', '-p', 'pancetta-qso', '--lib', 'pan72_stall_detection_tests'],
    ['cargo', 'test', '-p', 'pancetta', '--features', 'pancetta-ft8/transmit', '--lib', 'pan71_playback_start_tests'],
]
times = [[], []]
passed = 0
for iteration in range(iterations):
    ok = True
    for group, command in enumerate(commands):
        if load.poll() is not None:
            build_log.seek(0)
            print(build_log.read())
            sys.exit('Concurrent build ended before the load loop completed')
        start = time.monotonic()
        result = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        elapsed = time.monotonic() - start
        times[group].append(elapsed)
        print(f'PAN-325 run {iteration + 1} group {group + 1}: {elapsed:.3f}s, exit {result.returncode}', flush=True)
        if result.returncode:
            print(result.stdout, flush=True)
            ok = False
    passed += ok
print(f'PAN-325 load loop: {passed}/{iterations} passed', flush=True)
medians = [statistics.median(group) for group in times]
print(f'Median wall time: pan72={medians[0]:.3f}s (ceiling 10s), pan71={medians[1]:.3f}s (ceiling 30s)', flush=True)
sys.exit(0 if passed == iterations and medians[0] <= 10 and medians[1] <= 30 else 1)
PYTHON
