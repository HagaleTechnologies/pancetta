#!/usr/bin/env python3
"""PAN-315: enforce resolved TLS, direct-version and workspace MSRV policy."""
import json
from pathlib import Path
import subprocess
import tomllib

ROOT = Path(__file__).resolve().parents[2]

def check(target):
    command = ['cargo', 'metadata', '--locked', '--format-version', '1']
    if target:
        command += ['--filter-platform', target]
    graph = json.loads(subprocess.check_output(command, cwd=ROOT))
    packages = {p['id']: p for p in graph['packages']}
    nodes = {n['id']: n for n in graph['resolve']['nodes']}
    members = graph['workspace_members']
    reached = set()
    pending = list(members)
    while pending:
        key = pending.pop()
        if key in reached:
            continue
        reached.add(key)
        pending.extend(d['pkg'] for d in nodes[key]['deps'])
    versions = {}
    for key in reached:
        p = packages[key]
        versions.setdefault(p['name'], set()).add(p['version'])
    # PAN-441: chrono is unmaintained (RUSTSEC-2026-0543); the workspace uses jiff.
    for name in ['native-tls', 'tokio-native-tls', 'openssl', 'openssl-sys', 'chrono']:
        assert name not in versions, (target, name)
    for name in ['rustls', 'tokio-tungstenite', 'tungstenite', 'dirs', 'signal-hook']:
        assert len(versions.get(name, set())) == 1, (target, name, versions.get(name))
    assert len(members) == 14
    # PAN-384: the two crates.io crates promise registry users an older MSRV
    # (1.88, held by #![deny(clippy::incompatible_msrv)] in each lib.rs); every
    # other member inherits the workspace 1.98.
    registry_msrv = {'pancetta-core': '1.88', 'pancetta-ft8': '1.88'}
    for key in members:
        p = packages[key]
        expected = registry_msrv.get(p['name'], '1.98')
        assert p['rust_version'] == expected, (p['name'], p['rust_version'], expected)
    # Check resolved direct edges too: a lone transitive version must not
    # conceal a member switching its declaration to a second, unused line.
    aligned = {'tokio-tungstenite': '0.29.', 'dirs': '6.', 'signal-hook': '0.3.', 'sha2': '0.11.'}
    for key in members:
        for edge in nodes[key]['deps']:
            dependency = packages[edge['pkg']]
            prefix = aligned.get(dependency['name'])
            if prefix:
                assert dependency['version'].startswith(prefix), (packages[key]['name'], dependency['name'], dependency['version'])
    for key in reached:
        p = packages[key]
        if p['name'].startswith('sqlx'):
            assert not any('tls' in f for f in nodes[key]['features']), nodes[key]
            # PAN-441: sqlx's unused `chrono` feature stays off.
            assert 'chrono' not in nodes[key]['features'], nodes[key]
    print(f'{target or "all targets"}: dependency policy passed')

workspace = tomllib.loads((ROOT / 'Cargo.toml').read_text())['workspace']['dependencies']
assert not {'native-tls', 'tower-http', 'csv', 'tokio-util'}.intersection(workspace)
reqwest = workspace['reqwest']
assert reqwest['default-features'] is False
assert set(reqwest['features']) == {'rustls', 'charset', 'http2', 'system-proxy', 'json', 'stream', 'multipart', 'cookies', 'query', 'form'}
assert workspace['tokio-tungstenite']['version'] == '0.29'
assert workspace['tokio-tungstenite']['features'] == ['rustls-tls-native-roots']
removed = {
    'pancetta': ['metrics', 'tower-http', 'thiserror'],
    'pancetta-ft8': ['time', 'anyhow', 'num-traits', 'chrono'],
    'pancetta-dx': ['csv'],
    'pancetta-config': ['clap', 'validator'],
    'pancetta-core': ['thiserror'],
    'pancetta-hamlib': ['crossbeam-channel', 'num-traits', 'serde_json', 'tokio-util'],
    'pancetta-dsp': ['num-traits', 'rustfft'],
    'pancetta-tui': ['thiserror', 'tokio-util'],
}
for member, names in removed.items():
    manifest = tomllib.loads((ROOT / member / 'Cargo.toml').read_text())
    for section in ['dependencies', 'dev-dependencies', 'build-dependencies']:
        assert not set(names).intersection(manifest.get(section, {})), (member, section)
assert not (ROOT / 'pancetta-core/src/error.rs').exists()
core = (ROOT / 'pancetta-core/src/lib.rs').read_text()
assert 'pub mod error;' not in core and 'pub use error::' not in core
assert tomllib.loads((ROOT / 'pancetta-config/Cargo.toml').read_text())['features']['cli'] == []
for path in ['.catalyst/catalyst.toml', *[f'.github/workflows/{name}.yml' for name in ['ci', 'release', 'demo-assets']]]:
    assert 'libssl-dev' not in (ROOT / path).read_text(), path
# PAN-415: every workspace member that declares sha2, in any dependency
# section (target-specific tables included), declares the 0.11 line.
# sha2 0.10 may stay in the graph only through upstream crates (deny.toml skip).
def sha2_requirements(manifest):
    tables = [manifest]
    tables += list(manifest.get('target', {}).values())
    for table in tables:
        for section in ['dependencies', 'dev-dependencies', 'build-dependencies']:
            spec = table.get(section, {}).get('sha2')
            if spec is not None:
                if isinstance(spec, dict) and spec.get('workspace'):
                    spec = workspace.get('sha2', {})
                yield section, spec if isinstance(spec, str) else spec.get('version')

members = tomllib.loads((ROOT / 'Cargo.toml').read_text())['workspace']['members']
sha2_declarers = set()
for member in members:
    manifest = tomllib.loads((ROOT / member / 'Cargo.toml').read_text())
    for section, requirement in sha2_requirements(manifest):
        assert requirement is not None and requirement.startswith('0.11'), (member, section, requirement)
        sha2_declarers.add(member)
assert {'pancetta-agent', 'pancetta-ft8', 'pancetta-research'} <= sha2_declarers, sha2_declarers
for target in [None, 'x86_64-unknown-linux-gnu', 'aarch64-apple-darwin', 'x86_64-pc-windows-msvc']:
    check(target)
