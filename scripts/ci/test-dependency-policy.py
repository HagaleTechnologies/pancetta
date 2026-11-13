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
    for name in ['native-tls', 'tokio-native-tls', 'openssl', 'openssl-sys']:
        assert name not in versions, (target, name)
    for name in ['rustls', 'tokio-tungstenite', 'tungstenite', 'dirs', 'signal-hook']:
        assert len(versions.get(name, set())) == 1, (target, name, versions.get(name))
    assert len(members) == 14
    for key in members:
        p = packages[key]
        assert p['rust_version'] == '1.98', (p['name'], p['rust_version'])
    for key in reached:
        p = packages[key]
        if p['name'].startswith('sqlx'):
            assert not any('tls' in f for f in nodes[key]['features']), nodes[key]
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
    'pancetta-config': ['clap'],
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
for path in ['.catalyst/catalyst.toml', *[f'.github/workflows/{name}.yml' for name in ['ci', 'release', 'docs', 'demo-assets']]]:
    assert 'libssl-dev' not in (ROOT / path).read_text(), path
for target in [None, 'x86_64-unknown-linux-gnu', 'aarch64-apple-darwin', 'x86_64-pc-windows-gnu']:
    check(target)
