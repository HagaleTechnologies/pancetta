# Vendored Semgrep rulesets

`secrets.yaml` is the Semgrep Registry `p/secrets` pack, vendored into the
repository so token-absent runs (fork PRs, Dependabot PRs without a
provisioned Dependabot secret) can run a real, secret-free scan with rules
that are pinned and reviewed in-repo instead of fetched from the registry by
name at scan time (PAN-107; same pin-and-review doctrine as the SHA-pinned
actions in `supply-chain.yml`).

- **Origin**: <https://semgrep.dev/c/p/secrets> (served as plain `rules:` YAML)
- **Fetched**: 2026-10-18
- **Rules**: 52
- **SHA-256**:
  `d1afb323ed493cf9d3ee5044447b989debf033b2b7cc8208662a81b8c162c66a`
- **Consumed by**: `.github/workflows/semgrep.yml`, job `semgrep-local`
  (`semgrep scan --config .semgrep/secrets.yaml --error --metrics=off --exclude .semgrep`)

Findings printed from a local config file carry the config path as an ID
prefix (e.g. `semgrep.generic.secrets.…`); the suffix is the registry rule ID.

## Refreshing

Refresh only by PR, like any other dependency bump:

    curl -fsSL https://semgrep.dev/c/p/secrets -o .semgrep/secrets.yaml
    sha256sum .semgrep/secrets.yaml   # record the new digest above
    git diff .semgrep/secrets.yaml    # review the rule changes like a dep bump

Then verify locally (no token, no login needed):

    semgrep scan --config .semgrep/secrets.yaml --error --metrics=off --exclude .semgrep .   # clean tree: exit 0
