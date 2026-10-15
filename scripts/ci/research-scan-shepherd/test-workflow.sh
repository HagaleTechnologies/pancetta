#!/usr/bin/env bash
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
ruby -ryaml <<'RUBY'
w = YAML.load_file('.github/workflows/research-scan-shepherd.yml')
on = w.fetch('on') { w.fetch(true) }
raise 'triggers' unless on.keys.sort == %w[schedule workflow_dispatch]
raise 'cadence' unless on['schedule'] == [{'cron' => '17 2,14 * * *'}]
inputs = on.fetch('workflow_dispatch').fetch('inputs')
raise 'pr input' unless inputs['pr']['type'] == 'string' && inputs['pr']['default'] == ''
raise 'dry run input' unless inputs['dry_run']['type'] == 'boolean' && inputs['dry_run']['default'] == false
raise 'permissions' unless w['permissions'] == {'contents'=>'read','pull-requests'=>'write','issues'=>'write','actions'=>'write','checks'=>'read'}
raise 'concurrency' unless w['concurrency'] == {'group'=>'research-scan-shepherd','cancel-in-progress'=>false}
steps = w.fetch('jobs').fetch('shepherd').fetch('steps')
steps.each do |s|
  raise 'pin' if s['uses'] && !s['uses'].match?(/@[0-9a-f]{40}$/)
  raise 'injected expression' if s.fetch('run', '').include?('${{')
  (s['env'] || {}).each do |k,v|
    raise 'PAT scope' if v.include?('secrets.CODEX_REVIEW_PAT') && k != 'RESOLVE_TOKEN'
  end
end
text = File.read('.github/workflows/research-scan-shepherd.yml')
raise 'PAT exposed' unless text.scan('secrets.CODEX_REVIEW_PAT').length == 1
raise 'app key exposed' unless text.scan('secrets.HAGALE_AGENT_APP_PRIVATE_KEY').length == 1
mint = steps.find { |s| s['id'] == 'app-token' } or raise 'missing app-token mint'
raise 'app key scope' unless mint.dig('with', 'private-key') == '${{ secrets.HAGALE_AGENT_APP_PRIVATE_KEY }}'
raise 'checks token' unless steps.any? { |s| (s['env'] || {})['CHECKS_TOKEN'] == '${{ steps.app-token.outputs.token }}' }
raise 'environment' unless w['jobs']['shepherd']['environment'] == {'name'=>'codex-review','deployment'=>false}
puts 'PASS: workflow permissions, triggers, concurrency, pins and inputs'
RUBY
