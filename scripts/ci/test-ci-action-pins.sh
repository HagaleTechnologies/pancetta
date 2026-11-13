#!/usr/bin/env bash
# PAN-311: offline proof of the workflow-hygiene invariants, in four sections:
#
#   1. fleet scan   -- every `uses:` in .github/workflows/*.yml is a 40-hex
#                      commit SHA carrying a raw-line `#` version comment, and
#                      exactly one dtolnay/rust-toolchain SHA exists fleet-wide;
#   2. schedule     -- supply-chain.yml runs weekly (cron) plus on
#                      workflow_dispatch, the PR-gated jobs and the fan-in are
#                      guarded to pull_request/merge_group, and osv-scheduled
#                      runs a full OSV scan on schedule/dispatch with only
#                      SHA-pinned actions;
#   3. retirement   -- docs.yml and the five dead scripts are gone, with no
#                      references left outside docs/ narrative;
#   4. verifier     -- the actions-pins verifier, extracted textually from the
#                      shipped supply-chain.yml, fails on an added mutable tag
#                      (naming file:line), on an unchanged mutable tag under an
#                      empty diff, on a SHA pin without a version comment, and
#                      on a floating branch ref -- and passes on this tree.
#
# The verifier scenarios run the extracted Python exactly as the runner does
# (python3 <extracted> <base> <head>) inside scratch git repos seeded with the
# repository's real workflow files, so no GitHub round-trip is needed. CI picks
# this test up through the research-scan-tests job's existing
# scripts/ci/test-ci-*.sh glob (ci.yml); PyYAML is required because the
# extracted verifier itself imports yaml.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
for tool in ruby python3 git; do
  command -v "$tool" >/dev/null || { echo "FAIL: $tool is required" >&2; exit 1; }
done
if ! python3 -c 'import yaml' >/dev/null 2>&1; then
  echo "FAIL: the extracted verifier needs PyYAML (pip install pyyaml)" >&2
  exit 1
fi

WORK=$(mktemp -d)
VERIFIER="$WORK/action-pins.py"
export VERIFIER
trap 'rm -rf "$WORK"' EXIT

# --- Sections 1-3 (YAML structure) plus verifier extraction ------------------
struct_fail=0
ruby -ryaml <<'RUBY' || struct_fail=1
  Encoding.default_external = Encoding::UTF_8
  failures = []
  count = 0
  check = lambda do |name, &blk|
    blk.call
    count += 1
  rescue StandardError => e
    failures << "#{name}: #{e.message}"
  end
  expect = ->(cond, msg) { raise msg unless cond }

  workflows = Dir['.github/workflows/*.{yml,yaml}'].sort
  raise 'no workflow files under .github/workflows' if workflows.empty?

  # -- 1. fleet scan ---------------------------------------------------------
  sha_re = /\A[0-9a-f]{40}\z/
  uses_line_re = /\A\s*(?:-\s+)?uses:\s*(\S+)(.*)\z/
  dtolnay_shas = {}
  workflows.each do |path|
    doc = YAML.load_file(path)
    raise "#{path}: not a workflow mapping" unless doc.is_a?(Hash)
    (doc['jobs'] || {}).each do |job_id, job|
      next unless job.is_a?(Hash)
      refs = []
      refs << job['uses'] if job['uses'].is_a?(String)
      (job['steps'] || []).each do |st|
        refs << st['uses'] if st.is_a?(Hash) && st['uses'].is_a?(String)
      end
      refs.each do |ref|
        next if ref.start_with?('HagaleTechnologies/') # the fleet's own code: exempt
        check.call("fleet/#{path}/#{job_id}: #{ref}") do
          expect.call(ref.include?('@'), "#{path}: #{ref} has no @version")
          ver = ref.split('@').last
          expect.call(ver =~ sha_re, "#{path}: #{ref} is not pinned to a 40-hex commit SHA")
        end
        dtolnay_shas[ref.split('@').last] = true if ref.start_with?('dtolnay/rust-toolchain@')
      end
    end
    File.readlines(path).each_with_index do |raw, idx|
      m = uses_line_re.match(raw.chomp)
      next unless m
      val = m[1].gsub(/\A['"]|['"]\z/, '')
      next if val.start_with?('HagaleTechnologies/')
      next unless val.split('@').last =~ sha_re
      check.call("fleet-comment/#{path}:#{idx + 1}") do
        expect.call(m[2].include?('#'), "#{path}:#{idx + 1}: SHA-pinned `#{val}` carries no `#` version comment")
      end
    end
  end
  check.call('fleet/one dtolnay SHA') do
    want = ['686976e191b89faba57d3206551f0f330d8cb249']
    expect.call(dtolnay_shas.keys == want,
                "dtolnay/rust-toolchain SHAs are #{dtolnay_shas.keys.inspect}, want exactly #{want.inspect}")
  end

  # -- 2. schedule structure -------------------------------------------------
  sc = YAML.load_file('.github/workflows/supply-chain.yml')
  sc_jobs = sc.fetch('jobs')
  on = sc['on'] || sc[true]
  raise 'supply-chain.yml has no `on:` block' unless on.is_a?(Hash)
  check.call('schedule/weekly cron') do
    expect.call(on['schedule'] == [{ 'cron' => '23 4 * * 1' }],
                "on.schedule is #{on['schedule'].inspect}, want one weekly cron \"23 4 * * 1\"")
  end
  check.call('schedule/workflow_dispatch') do
    expect.call(on.key?('workflow_dispatch'), 'on.workflow_dispatch missing')
  end
  pr_mg = "github.event_name == 'pull_request' || github.event_name == 'merge_group'"
  %w[changes actions-pins].each do |jid|
    check.call("schedule/#{jid} PR/merge_group guarded") do
      got = sc_jobs.dig(jid, 'if')
      expect.call(got == pr_mg, "#{jid} if is #{got.inspect}, want #{pr_mg.inspect}")
    end
  end
  check.call('schedule/fan-in guarded and unchanged') do
    fi = sc_jobs.dig('supply-chain', 'if')
    expect.call(fi == "always() && (#{pr_mg})", "supply-chain if is #{fi.inspect}")
    expect.call(sc_jobs.dig('supply-chain', 'name') == 'Supply Chain',
                'fan-in job name must stay "Supply Chain" (the required status check)')
    want = %w[changes osv-pr osv-merge-group install-policy actions-pins]
    expect.call(sc_jobs.dig('supply-chain', 'needs') == want,
                "fan-in needs is #{sc_jobs.dig('supply-chain', 'needs').inspect}, want #{want.inspect}")
  end
  check.call('schedule/osv-scheduled full scan') do
    j = sc_jobs.fetch('osv-scheduled')
    want = "github.event_name == 'schedule' || github.event_name == 'workflow_dispatch'"
    expect.call(j['if'] == want, "osv-scheduled if is #{j['if'].inspect}")
    expect.call(j['permissions'] == { 'actions' => 'read', 'contents' => 'read', 'security-events' => 'write' },
                "osv-scheduled permissions is #{j['permissions'].inspect}")
    used = (j['steps'] || []).map { |st| st['uses'] if st.is_a?(Hash) }.compact
    expect.call(!used.empty?, 'osv-scheduled declares no uses at all')
    used.each do |ref|
      expect.call(ref.split('@').last =~ sha_re, "osv-scheduled uses #{ref}, which is not SHA-pinned")
    end
  end

  # -- 3. retirement ----------------------------------------------------------
  check.call('retire/docs.yml gone') do
    expect.call(!File.exist?('.github/workflows/docs.yml'), '.github/workflows/docs.yml still exists')
  end
  check.call('retire/main-alert no Docs entry') do
    ma = YAML.load_file('.github/workflows/main-alert.yml')
    ma_on = ma['on'] || ma[true]
    list = ma_on['workflow_run']['workflows']
    expect.call(!list.include?('Docs'), "main-alert workflow_run still lists Docs: #{list.inspect}")
  end
  check.call('retire/wiki does not mention docs.yml') do
    expect.call(!File.read('wiki/pages/config-and-platform.md').include?('docs.yml'),
                'wiki/pages/config-and-platform.md still mentions docs.yml')
  end
  %w[run_integration_tests.sh run_stability_test.sh run_performance_tests.sh five-minute-drill.sh build_release.sh].each do |s|
    check.call("retire/scripts/#{s} gone") do
      expect.call(!File.exist?("scripts/#{s}"), "scripts/#{s} still exists")
    end
  end
  check.call('retire/no stray script references') do
    found = `git grep -l -F -e run_integration_tests.sh -e run_stability_test.sh -e run_performance_tests.sh -e five-minute-drill.sh -e build_release.sh 2>/dev/null`
    stray = found.split("\n").reject { |p| p.start_with?('docs/') || p == 'scripts/ci/test-ci-action-pins.sh' }
    expect.call(stray.empty?, "retired scripts still referenced outside docs/: #{stray.join(', ')}")
  end
  check.call('retire/no docs.yml references under .github or wiki') do
    found = `git grep -l -F docs.yml -- .github wiki 2>/dev/null`
    expect.call(found.split("\n").empty?, "docs.yml still referenced: #{found.split("\n").join(', ')}")
  end

  # -- 4. verifier extraction -------------------------------------------------
  step = (sc_jobs.fetch('actions-pins')['steps'] || []).find do |st|
    st.is_a?(Hash) && st['run'].to_s.include?("cat > /tmp/action-pins.py <<'PY'")
  end
  raise 'actions-pins step carrying the verifier heredoc not found' unless step
  lines = step['run'].split("\n")
  start = lines.index { |l| l.start_with?("cat > /tmp/action-pins.py <<'PY'") }
  raise 'verifier heredoc start marker not found' unless start
  py = []
  i = start + 1
  while i < lines.length && lines[i].strip != 'PY'
    py << lines[i]
    i += 1
  end
  raise 'verifier heredoc terminator PY not found' if i >= lines.length
  body = py.join("\n") + "\n"
  raise 'extracted verifier does not look like the actions-pins verifier' unless body.include?('refs_at') && body.include?('action pin check')
  File.write(ENV.fetch('VERIFIER'), body)
  count += 1

  unless failures.empty?
    warn failures.map { |f| "FAIL: #{f}" }.join("\n")
    warn "#{failures.size} structure check(s) failed, #{count} passed"
    exit 1
  end
  puts "PASS: #{count} structure checks (PAN-311)"
RUBY

# --- Section 4: verifier scenarios -------------------------------------------
[ -s "$VERIFIER" ] || { echo "FAIL: verifier extraction did not produce a file" >&2; exit 1; }

fails=0
passes=0
ok()  { passes=$((passes + 1)); echo "PASS: $1"; }
bad() { fails=$((fails + 1)); echo "FAIL: $1: $2" >&2; }

# Seeds a scratch git repo (named $1) with this repository's real workflow
# files; sets SCRATCH and prints nothing.
new_scratch() {
  SCRATCH="$WORK/scratch-$1"
  rm -rf "$SCRATCH"
  mkdir -p "$SCRATCH/.github/workflows"
  cp .github/workflows/*.yml "$SCRATCH/.github/workflows/"
  git -C "$SCRATCH" init -q -b main
  git -C "$SCRATCH" config user.email ci@example.invalid
  git -C "$SCRATCH" config user.name "CI offline test"
  git -C "$SCRATCH" add -A
  git -C "$SCRATCH" commit -q -m seed
}

# Runs the extracted verifier in $SCRATCH as the runner does.
run_verifier() { # $1 = base, $2 = head
  (cd "$SCRATCH" && python3 "$VERIFIER" "$1" "$2" 2>&1)
}

commit_scratch() {
  git -C "$SCRATCH" add -A
  git -C "$SCRATCH" commit -q -m "$1"
  git -C "$SCRATCH" rev-parse HEAD
}

# Scenario: fleet -- the verifier passes on this tree's real workflows, the
# combined pin-conversion + strict-gate state CI will actually run.
new_scratch fleet
base=$(git -C "$SCRATCH" rev-parse HEAD)
out=$(run_verifier "$base" "$base") && rc=0 || rc=$?
if [ "$rc" -eq 0 ]; then
  ok "scenario/fleet-passes-at-head"
else
  bad "scenario/fleet-passes-at-head" "verifier exit=$rc on the shipped tree: $out"
fi

# Scenario: added-tag-fails-naming-file-and-line (the ticket's Gherkin) -- a PR
# whose only change adds `uses: actions/checkout@v7` fails naming file and line.
new_scratch added-tag
base=$(git -C "$SCRATCH" rev-parse HEAD)
cat > "$SCRATCH/.github/workflows/zz-scratch-tag.yml" <<'YML'
name: Scratch tag pin
on: workflow_dispatch
permissions: {}
jobs:
  scratch:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v7
YML
head=$(commit_scratch "add tag-pinned step")
uses_line=$(grep -n "actions/checkout@v7" "$SCRATCH/.github/workflows/zz-scratch-tag.yml" | cut -d: -f1)
out=$(run_verifier "$base" "$head") && rc=0 || rc=$?
if [ "$rc" -ne 0 ] && printf '%s\n' "$out" | grep -Fq "zz-scratch-tag.yml:${uses_line}"; then
  ok "scenario/added-tag-fails-naming-file-and-line"
else
  bad "scenario/added-tag-fails-naming-file-and-line" "verifier exit=$rc, expected non-zero naming zz-scratch-tag.yml:${uses_line}; output: $out"
fi

# Scenario: unchanged-tag-fails-empty-diff -- the verifier judges every ref at
# HEAD, so a mutable tag no diff touches still fails with BASE == HEAD.
out=$(run_verifier "$head" "$head") && rc=0 || rc=$?
if [ "$rc" -ne 0 ]; then
  ok "scenario/unchanged-tag-fails-empty-diff"
else
  bad "scenario/unchanged-tag-fails-empty-diff" "verifier exit=0 on an unchanged mutable tag (BASE == HEAD): $out"
fi

# Scenario: sha-without-comment-fails -- a SHA pin without a trailing version
# comment fails the gate, naming file:line.
new_scratch no-comment
base=$(git -C "$SCRATCH" rev-parse HEAD)
cat > "$SCRATCH/.github/workflows/zz-scratch-nocomment.yml" <<'YML'
name: Scratch SHA without comment
on: workflow_dispatch
permissions: {}
jobs:
  scratch:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1
YML
head=$(commit_scratch "add SHA-pinned step without comment")
uses_line=$(grep -n "actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1" "$SCRATCH/.github/workflows/zz-scratch-nocomment.yml" | cut -d: -f1)
out=$(run_verifier "$base" "$head") && rc=0 || rc=$?
if [ "$rc" -ne 0 ] && printf '%s\n' "$out" | grep -Fq "zz-scratch-nocomment.yml:${uses_line}"; then
  ok "scenario/sha-without-comment-fails"
else
  bad "scenario/sha-without-comment-fails" "verifier exit=$rc, expected non-zero naming zz-scratch-nocomment.yml:${uses_line}; output: $out"
fi

# Scenario: floating-ref-still-fails -- regression: a branch ref fails hard.
new_scratch floating
base=$(git -C "$SCRATCH" rev-parse HEAD)
cat > "$SCRATCH/.github/workflows/zz-scratch-floating.yml" <<'YML'
name: Scratch floating ref
on: workflow_dispatch
permissions: {}
jobs:
  scratch:
    runs-on: ubuntu-latest
    steps:
      - uses: evil/action@main
YML
head=$(commit_scratch "add floating-ref step")
out=$(run_verifier "$base" "$head") && rc=0 || rc=$?
if [ "$rc" -ne 0 ]; then
  ok "scenario/floating-ref-still-fails"
else
  bad "scenario/floating-ref-still-fails" "verifier exit=0 on evil/action@main: $out"
fi

if [ "$struct_fail" -ne 0 ] || [ "$fails" -ne 0 ]; then
  echo "$fails scenario check(s) failed (structure_fail=$struct_fail)" >&2
  exit 1
fi
echo "PASS: PAN-311 workflow-hygiene invariants ($passes verifier scenarios plus all structure checks)"
