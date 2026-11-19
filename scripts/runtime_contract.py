"""AUTO-16 Wave 0 runtime contract adapter and fixture conformance self-test.

This module is intentionally a test adapter. Release validators and installers remain unchanged.
"""
import json
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from release_component_identity import version
import release_format

ROOT = Path(__file__).resolve().parents[1]
FIXTURE = ROOT / 'scripts' / 'fixtures' / 'release-contract.json'
class ContractError(ValueError):
    def __init__(self, code): self.code = code; super().__init__(code)

def load_runtime(path=FIXTURE):
    data = json.loads(Path(path).read_text(encoding='utf-8'))
    runtime = data.get('runtime')
    if not isinstance(runtime, dict) or runtime.get('schema') != 1: raise ContractError('schema')
    return runtime

def duplicate_pairs(pairs):
    out = {}
    for key, value in pairs:
        if key in out: raise ContractError('json-duplicate')
        out[key] = value
    return out

def check_build_info(value, runtime):
    if not isinstance(value, dict): raise ContractError('object')
    fields = runtime['build_info']['fields']
    if sorted(value) != sorted(fields): raise ContractError('fields')
    if value['target'] not in {row['target'] for row in runtime['targets']}: raise ContractError('target')
    for field in ('source_commit', 'core_source_commit', 'app_source_commit'):
        if not isinstance(value[field], str) or not re.fullmatch(r'[a-f0-9]{40}', value[field]): raise ContractError('commit')
    for field in ('data_epoch', 'update_protocol'):
        expected = runtime['contracts'][field]
        if type(value[field]) is not int or not 0 <= value[field] <= 0xffffffff or value[field] != expected: raise ContractError('contract')
    for field in ('version', 'core_version', 'app_version'):
        try: version(value[field])
        except (TypeError, ValueError, OverflowError): raise ContractError('version')
    return True

def check_build_raw(raw, runtime):
    for field in ('data_epoch', 'update_protocol'):
        match = re.search(rf'"{field}"\s*:\s*([^,}}\s]+)', raw)
        if not match or not re.fullmatch(r'(0|[1-9][0-9]*)', match.group(1)): raise ContractError('contract')
    try: value = json.loads(raw, object_pairs_hook=duplicate_pairs)
    except ContractError: raise
    except (TypeError, ValueError): raise ContractError('json')
    return check_build_info(value, runtime)

HASH = re.compile(r'^[a-f0-9]{64}$')
SAFE_PATH = re.compile(r'^[A-Za-z0-9._/-]+$')
def check_files(raw):
    if not isinstance(raw, str) or raw.startswith('\ufeff') or not raw.endswith('\n'): raise ContractError('syntax')
    lines = raw[:-1].split('\n')
    if not lines or any(not line for line in lines): raise ContractError('syntax')
    paths = []
    for line in lines:
        match = re.fullmatch(r'([^ ]+)  (.+)', line)
        if not match or not HASH.fullmatch(match[1]): raise ContractError('syntax')
        name = match[2]
        if name != name.strip(): raise ContractError('syntax')
        if name == 'FILES.sha256': raise ContractError('self-entry')
        if (name.startswith('/') or '\\' in name or any(part in ('', '.', '..') for part in name.split('/')) or not SAFE_PATH.fullmatch(name)): raise ContractError('path')
        paths.append(name)
    if len(set(paths)) != len(paths): raise ContractError('duplicate')
    if sorted(paths, key=lambda item: item.encode()) != paths: raise ContractError('order')
    return True

def check_role(files, target, role, runtime):
    row = next((row for row in runtime['targets'] if row['target'] == target), None)
    if row is None or role not in row['roles']: raise ContractError('role')
    if not isinstance(files, list) or not all(name in files for name in row['roles'][role]): raise ContractError('required')
    return True

def check_seal(item):
    if item.get('symlink'): raise ContractError('symlink')
    if item.get('special'): raise ContractError('special')
    if item.get('changed'): raise ContractError('changed')
    listed, actual = set(item['listed']), set(item['actual'])
    if listed - actual: raise ContractError('missing')
    if actual - listed: raise ContractError('unlisted')
    return True

def run_runtime_corpus(runtime=None):
    runtime = runtime or load_runtime()
    outcomes = []
    def run(item, operation):
        accepted, error = True, 'ok'
        try: operation()
        except ContractError as exc: accepted, error = False, exc.code
        except Exception: accepted, error = False, 'error'
        result = {'id': item['id'], 'accepted': accepted, 'error': error}
        if accepted != item['accepted'] or error != item['error']:
            raise AssertionError(f"{item['id']}: expected {item['accepted']}/{item['error']}, got {accepted}/{error}")
        outcomes.append(result)
    for item in runtime['cases']['build_info']:
        run(item, lambda item=item: check_build_raw(item['raw'], runtime) if item['kind'] == 'build-info-raw' else check_build_info(item['input'], runtime))
    for item in runtime['cases']['files']: run(item, lambda item=item: check_files(item['raw']))
    for item in runtime['cases']['roles']: run(item, lambda item=item: check_role(item['files'], item['target'], item['role'], runtime))
    for item in runtime['cases']['seals']: run(item, lambda item=item: check_seal(item))
    def check_wire(item):
        if item['wire'] != runtime['wire']: raise ContractError('wire')
    for item in runtime['cases']['compatibility']: run(item, lambda item=item: check_wire(item))
    for item in runtime['cases']['source']: run(item, lambda item=item: check_rust_contracts(item['source'], runtime))
    return outcomes

def strip_rust_comments(source):
    return re.sub(r'/\*[\s\S]*?\*/', '', re.sub(r'//[^\r\n]*', '', source))

def check_rust_contracts(source, runtime):
    active = strip_rust_comments(source)
    for field, key in (('UPDATE_PROTOCOL', 'update_protocol'), ('DATA_EPOCH', 'data_epoch')):
        matches = []
        for match in re.finditer(rf'^\s*pub\s+const\s+{field}\s*:\s*u32\s*=\s*([0-9]+)\s*;', active, re.MULTILINE):
            previous = list(reversed(active[:match.start()].split('\n')[:-1]))[:32]
            if not any(re.match(r'^\s*#', line) for line in previous): matches.append(match)
        if len(matches) != 1 or int(matches[0].group(1)) != runtime['contracts'][key]: raise ContractError('source')
    return True

def validate_runtime(runtime):
    expected_fields = ['version', 'target', 'source_commit', 'update_protocol', 'data_epoch', 'core_version', 'core_source_commit', 'app_version', 'app_source_commit']
    assert runtime['build_info']['fields'] == expected_fields
    web = json.loads((ROOT / 'scripts' / 'package-node-web.json').read_text())
    assert runtime['files']['web_required'] == web
    assert tuple(row['target'] for row in runtime['targets']) == release_format.TARGETS
    assert {row['target']: row['installer'] for row in runtime['targets']} == release_format.DESKTOP_INSTALLERS
    for row in runtime['targets']:
        assert row['programs'] == list(release_format.program_names(row['target']))
        assert row['roles']['node-full'] == ['FILES.sha256'] + list(release_format.program_names(row['target'])) + ['BUILD-INFO.json', 'README.md', 'RUNTIME-REQUIREMENTS.md', f"scripts/install-node-service.{row['service_suffix']}", 'docs/node.md', 'docs/releasing.md'] + ['web/' + name for name in web]
        assert row['roles']['npm-vendor'] == row['roles']['node-full']
        assert row['roles']['runtime-core'] == list(release_format.program_names(row['target'])) + ['BUILD-INFO.json', 'FILES.sha256']
    source = (ROOT / 'crates' / 'control' / 'src' / 'lib.rs').read_text()
    check_rust_contracts(source, runtime)
    return True

def main():
    runtime = load_runtime(); validate_runtime(runtime); outcomes = run_runtime_corpus(runtime)
    if '--json' in sys.argv: print(json.dumps(outcomes, separators=(',', ':')))
    else: print(f'Runtime contract corpus passed: {len(outcomes)} literal Python cases; target, wire, BUILD-INFO, FILES.sha256, roles and seal invariants.')

if __name__ == '__main__': main()
