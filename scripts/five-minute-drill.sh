#!/usr/bin/env bash
# five-minute-drill.sh — stopwatch for pancetta's post-build onboarding path.
#
# The operator's 5-minute goal, measured from the moment the build finishes:
# a new user configures the station and is ON THE AIR (rig connected, able
# to TX a QSO) within 5:00. Run this with a stopwatch mindset on a machine
# with a freshly built binary, the rig on USB, and NO ~/.pancetta config
# (the script offers to move an existing one aside).
#
# PASS: wizard + doctor-green + first decode within 300 s.
#       (TX-capable = the rig step was completed and doctor's rig check PASSes.)
# FAIL: any step dead-ends, or total time > 300 s.
set -u

BIN="${PANCETTA_BIN:-./target/release/pancetta}"
LIMIT=300
[ -x "$BIN" ] || { echo "ERROR: $BIN not found — cargo build --release first."; exit 2; }

CFG="$HOME/.pancetta/pancetta.toml"
if [ -f "$CFG" ]; then
  printf "Existing config found. Move it aside for a clean drill? [y/N] "
  read -r ans
  if [ "${ans:-n}" = "y" ]; then
    mv "$CFG" "$CFG.drill-backup.$(date +%s)"
    echo "Moved aside (restore from $CFG.drill-backup.*)."
  else
    echo "Running against the existing config (wizard step will be skipped)."
  fi
fi

START=$(date +%s)
elapsed() { echo $(( $(date +%s) - START )); }
mark() { printf "\n== [T+%03ds] %s ==\n" "$(elapsed)" "$1"; }

mark "STEP 1/4: first-run wizard (station -> audio -> RIG: answer y!)"
echo "Complete the wizard, then QUIT pancetta (q, y). Starting it now..."
"$BIN"

mark "STEP 2/4: pancetta doctor"
if "$BIN" doctor; then
  echo "doctor: GREEN"
else
  mark "RESULT: FAIL — doctor is red. Apply the printed fixes and re-run."
  exit 1
fi

mark "STEP 3/4: decode check"
echo "Starting pancetta. Rig on an FT8 frequency (e.g. 14.074 USB)."
echo "As soon as you SEE A DECODE in Band Activity, quit (q, y)."
"$BIN"

mark "STEP 4/4: operator attestation"
printf "Did you see at least one decode? [y/N] "; read -r saw
printf "Is the RIG badge connected (CAT working, TX-capable)? [y/N] "; read -r rig

TOTAL=$(elapsed)
echo
echo "-------------------------------------------"
echo " Total time: ${TOTAL}s (limit ${LIMIT}s)"
if [ "${saw:-n}" = "y" ] && [ "${rig:-n}" = "y" ] && [ "$TOTAL" -le "$LIMIT" ]; then
  echo " RESULT: PASS — on the air in under 5 minutes."
  exit 0
fi
echo " RESULT: FAIL"
[ "${saw:-n}" != "y" ] && echo "   - no decode observed (doctor + Shift+D to diagnose)"
[ "${rig:-n}" != "y" ] && echo "   - rig not connected/TX-capable (pancetta test-rig)"
[ "$TOTAL" -gt "$LIMIT" ] && echo "   - over the 300 s budget: find the slow step above (T+ marks)"
exit 1
