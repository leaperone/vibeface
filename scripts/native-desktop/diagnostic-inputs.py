#!/usr/bin/env python3
"""Verify and select existing CI Windows installer inputs without extracting profiles."""
import argparse
import datetime
import importlib.util
import io
import json
import os
from pathlib import Path
import re
import stat
import tempfile
import subprocess
import sys
import zipfile

ROOT = Path(__file__).resolve().parents[2]
TARGET = 'x86_64-pc-windows-msvc'
LIMIT = 1024 * 1024 * 1024
sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location('release', ROOT / 'scripts/release.py')
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)


def require(condition, message):
    if not condition:
        raise ValueError(message)


def github(path, binary=False):
    result = subprocess.run(['gh', 'api', path], stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False)
    require(result.returncode == 0, 'GitHub API request failed; credentials and redirect URLs are not retained')
    return result.stdout if binary else json.loads(result.stdout)


def validate_run(run, repository, run_id, source, attempt):
    require(str(run.get('id')) == run_id and run.get('head_sha') == source, 'source run or SHA mismatch')
    require(run.get('path') == '.github/workflows/ci.yml' and run.get('event') == 'workflow_dispatch', 'inputs must come from manual ci.yml')
    require(run.get('repository', {}).get('full_name') == repository
            and run.get('head_repository', {}).get('full_name') == repository, 'source run belongs to another repository')
    require(run.get('run_attempt') == attempt and run.get('status') == 'completed', 'source attempt must be exact and completed')


def selected_artifact(rows, name, run, source):
    matches = [item for item in rows if item.get('name') == name]
    require(len(matches) == 1, 'expected one exact immutable artifact: ' + name)
    item = matches[0]
    require(not item.get('expired') and 0 < item.get('size_in_bytes', 0) <= LIMIT, 'source artifact unavailable or oversized')
    require(item.get('workflow_run', {}).get('id') == run['id']
            and item.get('workflow_run', {}).get('head_sha') == source, 'artifact source run identity mismatch')
    created = datetime.datetime.fromisoformat(item['created_at'].replace('Z', '+00:00'))
    started = datetime.datetime.fromisoformat(run['run_started_at'].replace('Z', '+00:00'))
    ended = datetime.datetime.fromisoformat(run['updated_at'].replace('Z', '+00:00'))
    require(started <= created <= ended, 'artifact was not produced during the selected attempt')
    return item


def extract_selected(data, destination, select):
    require(len(data) <= LIMIT, 'artifact archive exceeds diagnostic size limit')
    destination.mkdir(parents=True)
    selected = []
    with zipfile.ZipFile(io.BytesIO(data)) as archive:
        for item in archive.infolist():
            name = select(item.filename)
            if name is None:
                continue  # Prior diagnostic profiles are never extracted or uploaded.
            require(name == Path(name).name and re.fullmatch(r'[A-Za-z0-9._-]+', name), 'unsafe selected artifact name')
            require(name not in selected and not item.is_dir()
                    and stat.S_IFMT(item.external_attr >> 16) != stat.S_IFLNK
                    and not item.flag_bits & 1 and item.file_size <= release.MAX_UNPACKED, 'invalid selected artifact member')
            with archive.open(item) as source, (destination / name).open('xb') as target:
                while block := source.read(1024 * 1024):
                    target.write(block)
            selected.append(name)
    require(selected, 'source artifact contains no expected installer inputs')
    return selected


def prepare(args):
    require(re.fullmatch(r'[1-9][0-9]*', args.source_run) and re.fullmatch(r'[a-f0-9]{40}', args.source_sha)
            and re.fullmatch(r'[1-9][0-9]*', args.source_attempt), 'invalid run, full source SHA or attempt')
    require(os.environ.get('GITHUB_ACTIONS') == 'true' and os.environ.get('RUNNER_ENVIRONMENT') == 'github-hosted'
            and sys.platform == 'win32', 'diagnostic inputs are prepared only on hosted Windows')
    repository = os.environ['GITHUB_REPOSITORY']
    require(re.fullmatch(r'[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+', repository), 'invalid repository')
    attempt = int(args.source_attempt)
    run = github(f'repos/{repository}/actions/runs/{args.source_run}/attempts/{attempt}')
    validate_run(run, repository, args.source_run, args.source_sha, attempt)
    rows, page = [], 1
    while True:
        result = github(f'repos/{repository}/actions/runs/{args.source_run}/artifacts?per_page=100&page={page}')
        rows.extend(result['artifacts'])
        if len(result['artifacts']) < 100:
            break
        page += 1
    desktop = selected_artifact(rows, f'desktop-{TARGET}-{args.source_sha}-{attempt}', run, args.source_sha)
    runtime = selected_artifact(rows, f'desktop-runtime-{TARGET}-{args.source_sha}', run, args.source_sha)
    require(not args.output.exists(), 'diagnostic input output must be fresh')
    args.output.mkdir(parents=True)
    desktop_dir, runtime_dir = args.output / 'desktop', args.output / 'runtime-input'
    def desktop_member(name):
        prefix = '.artifacts/desktop/'
        if not name.startswith(prefix):
            return None
        leaf = name[len(prefix):]
        expected = r'vibeface-desktop-' + release.VERSION + '-' + TARGET + r'\.(?:exe(?:\.sha256)?|zip(?:\.sha256)?|provenance\.json)'
        return leaf if re.fullmatch(expected, leaf) else None
    extract_selected(github(f'repos/{repository}/actions/artifacts/{desktop["id"]}/zip', True), desktop_dir, desktop_member)
    receipts = list(desktop_dir.glob('*.provenance.json'))
    require(len(receipts) == 1, 'expected exactly one desktop receipt')
    receipt = json.loads(receipts[0].read_text(encoding='utf-8'))
    version = receipt['version']
    channel = release.version(version)
    names = release.desktop_asset_names(version, TARGET)
    require({p.name for p in desktop_dir.iterdir()} == names, 'unexpected or incomplete selected desktop payloads')
    archive_name = f'vibeface-{version}-{TARGET}.tar.gz'
    extract_selected(github(f'repos/{repository}/actions/artifacts/{runtime["id"]}/zip', True), runtime_dir,
                     lambda name: name if name in {archive_name, archive_name + '.sha256'} else None)
    require({p.name for p in runtime_dir.iterdir()} == {archive_name, archive_name + '.sha256'}, 'missing matching runtime archive')
    release.verify_checksum_file(runtime_dir, archive_name)
    release.verify_desktop_receipt(desktop_dir, TARGET,
        {'version': version, 'tag': 'v' + version, 'channel': channel, 'source_commit': args.source_sha},
        release.digest(runtime_dir / archive_name))
    bundle = release.unpack(runtime_dir / archive_name, args.output / 'runtime')
    info = json.loads((bundle / 'BUILD-INFO.json').read_text(encoding='utf-8'))
    release.verify_build_info(info)
    require(info['source_commit'] == args.source_sha and info['version'] == version and info['target'] == TARGET,
            'runtime cohort mismatch')
    content = release.files(bundle, ('FILES.sha256',))
    require((bundle / 'FILES.sha256').read_text(encoding='utf-8') == ''.join(f'{digest}  {name}\n' for name, digest in content.items()), 'runtime seal mismatch')
    portable = args.output / 'portable'
    with zipfile.ZipFile(desktop_dir / f'vibeface-desktop-{version}-{TARGET}.zip') as archive:
        require(sum(x.file_size for x in archive.infolist()) <= release.MAX_UNPACKED, 'portable package exceeds size limit')
        seen = set()
        for member in archive.infolist():
            name = member.filename.rstrip('/')
            release.safe_name(name)
            require(all(not part.endswith('.') and not re.match(r'^(con|prn|aux|nul|com[1-9]|lpt[1-9])(?:\.|$)', part, re.I) for part in name.split('/')), 'reserved Windows portable path')
            require(name.casefold() not in seen and not member.flag_bits & 1, 'duplicate or encrypted portable path')
            seen.add(name.casefold())
            require(stat.S_IFMT(member.external_attr >> 16) in {0, stat.S_IFREG, stat.S_IFDIR}, 'portable links or special entries are refused')
        archive.extractall(portable)
    require((portable / 'Vibeface.exe').is_file(), 'portable native executable missing')
    require(release.files(portable / 'resources/runtime') == release.files(bundle), 'portable runtime differs from sealed runtime')
    ownership_path = portable / '.vibeface-installed-files.json'
    ownership = json.loads(ownership_path.read_text(encoding='utf-8'))
    require(ownership.get('schema') == 1 and ownership.get('app') == 'vibeface-native-desktop', 'unsupported portable ownership manifest')
    all_files = release.files(portable)
    require(isinstance(ownership.get('files'), list) and len(ownership['files']) == len(set(ownership['files']))
            and set(ownership['files']) == set(all_files), 'portable ownership list differs from verified payload files')
    folders = set()
    for name in ownership['files']:
        release.safe_name(name)
        for parent in Path(name).parents:
            if parent.as_posix() != '.':
                folders.add(parent.as_posix())
    require(isinstance(ownership.get('directories'), list) and len(ownership['directories']) == len(folders)
            and set(ownership['directories']) == folders, 'portable ownership directory list differs from verified payload')
    result = {'source_run': args.source_run, 'source_sha': args.source_sha, 'source_attempt': attempt,
              'desktop_artifact_id': desktop['id'], 'runtime_artifact_id': runtime['id'],
              'portable_sha256': receipt['artifacts'][f'vibeface-desktop-{version}-{TARGET}.zip'],
              'runtime_archive_sha256': release.digest(runtime_dir / archive_name),
              'portable_ownership_sha256': release.digest(ownership_path),
              'runtime': str(bundle.resolve()), 'portable_app': str((portable / 'Vibeface.exe').resolve()),
              'installer': str((desktop_dir / f'vibeface-desktop-{version}-{TARGET}.exe').resolve())}
    args.evidence.parent.mkdir(parents=True, exist_ok=True)
    args.evidence.write_text(json.dumps(result, indent=2) + '\n', encoding='utf-8')
    with open(os.environ['GITHUB_OUTPUT'], 'a', encoding='utf-8') as output:
        for key in ['runtime', 'portable_app', 'installer']:
            output.write(f'{key}={result[key]}\n')


def self_test():
    repository = 'leaperone/vibeface'
    source = 'a' * 40
    run = {'id': 123, 'head_sha': source, 'path': '.github/workflows/ci.yml', 'event': 'workflow_dispatch',
           'repository': {'full_name': repository}, 'head_repository': {'full_name': repository},
           'run_attempt': 1, 'status': 'completed', 'run_started_at': '2026-11-11T00:00:00Z', 'updated_at': '2026-11-11T01:00:00Z'}
    validate_run(run, repository, '123', source, 1)
    for field, value in [('head_sha','b'*40), ('event','schedule'), ('path','.github/workflows/release.yml'), ('run_attempt',2)]:
        altered = dict(run, **{field:value})
        try: validate_run(altered, repository, '123', source, 1)
        except ValueError: pass
        else: raise AssertionError('accepted wrong source identity: '+field)
    item = {'id':12,'name':'exact','expired':False,'size_in_bytes':100,'created_at':'2026-11-11T00:30:00Z',
            'workflow_run':{'id':123,'head_sha':source}}
    assert selected_artifact([item], 'exact', run, source)['id'] == 12
    for rows in [[], [item,item], [dict(item,expired=True)], [dict(item,created_at='2026-11-10T00:30:00Z')]]:
        try: selected_artifact(rows, 'exact', run, source)
        except ValueError: pass
        else: raise AssertionError('accepted ambiguous, expired or wrong-attempt artifact')
    data = io.BytesIO()
    with zipfile.ZipFile(data, 'w') as archive:
        archive.writestr('.artifacts/desktop/payload.zip', 'literal fixture payload')
        archive.writestr('.artifacts/native-desktop-acceptance/profile/private.json', 'must not extract')
    with tempfile.TemporaryDirectory(prefix='native-input-filter-') as directory:
        destination = Path(directory) / 'selected'
        assert extract_selected(data.getvalue(), destination,
            lambda name: 'payload.zip' if name == '.artifacts/desktop/payload.zip' else None) == ['payload.zip']
        assert sorted(path.name for path in destination.iterdir()) == ['payload.zip']
        assert (destination / 'payload.zip').read_text() == 'literal fixture payload'
    print('Diagnostic input self-test passed: exact run/attempt identity, immutable artifact selection and no profile extraction')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--self-test', action='store_true')
    parser.add_argument('--source-run')
    parser.add_argument('--source-sha')
    parser.add_argument('--source-attempt')
    parser.add_argument('--output', type=Path)
    parser.add_argument('--evidence', type=Path)
    args = parser.parse_args()
    if args.self_test:
        self_test()
    else:
        if any(value is None for value in [args.source_run,args.source_sha,args.source_attempt,args.output,args.evidence]):
            parser.error('source run, SHA, attempt, output and evidence are required')
        try:
            prepare(args)
        except Exception as error:
            args.evidence.parent.mkdir(parents=True, exist_ok=True)
            args.evidence.with_name('input-validation-error.json').write_text(json.dumps(
                {'phase':'input-validation','error_type':type(error).__name__,'message':str(error)[:1024]}, indent=2)+'\n')
            raise

