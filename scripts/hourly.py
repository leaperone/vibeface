#!/usr/bin/env python3
"""Release changed applications and wake exact dependency consumers independently."""
import argparse
import datetime
import fnmatch
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import time

import release

COORDINATOR_SCHEMA = 2
REPO = release.GITHUB_REPO
BOT = '41898282+github-actions[bot]@users.noreply.github.com'
RUST = {'core': 'vibeface-core', 'cli': 'vibeface', 'desktop': 'vibeface-desktop'}
INPUTS = {
    'core': ('apps/core/*', 'crates/protocol/*', 'crates/control/*', 'crates/bridge/*', 'crates/secure/*', 'crates/client/*', 'crates/delivery/*', 'Cargo.toml', 'Cargo.lock', '.cargo/*', 'config/*', 'rust-toolchain*', 'scripts/sign-release.sh', 'scripts/release-rustflags.py', '.github/workflows/release-native.yml'),
    'app': ('apps/app/*', 'packages/client/*', 'packages/theme/*', 'crates/secure/*', 'Cargo.toml', 'Cargo.lock', 'rust-toolchain*', 'pnpm*', 'package.json', 'scripts/build-web*', 'scripts/build-inputs.py'),
    'cli': ('apps/cli/*', 'crates/control/*', 'crates/client/*', 'crates/bridge/*', 'crates/secure/*', 'crates/protocol/*', 'Cargo.toml', 'Cargo.lock', '.cargo/*', 'rust-toolchain*', 'install.sh', 'scripts/install*', 'scripts/package-*', 'scripts/offline-docs*', 'packages/docs/*'),
    'desktop': ('apps/desktop/*', 'crates/client/*', 'crates/secure/*', 'crates/bridge/*', 'crates/control/*', 'crates/protocol/*', 'Cargo.toml', 'Cargo.lock', '.cargo/*', 'scripts/build-desktop.mjs', 'scripts/verify-desktop.mjs', 'scripts/zip-directory.py', 'scripts/windows-signing.mjs', 'scripts/import-mac-identity.sh', '.github/actions/native-desktop/*'),
    'mobile': ('apps/mobile/*', 'packages/client/*', 'packages/theme/*', 'packages/secure-native/*', 'crates/secure/*', 'crates/secure-ffi/*', 'Cargo.toml', 'Cargo.lock', 'rust-toolchain*', 'pnpm*', 'package.json', 'scripts/build-mobile*', 'scripts/build-ios*', 'scripts/verify-ios*', '.github/workflows/release-apps.yml', '.github/workflows/release-ios.yml'),
    'www': ('apps/www/*', 'packages/docs/*', 'packages/theme/*', 'pnpm*', 'package.json'),
}
COMMON = ('scripts/release.py', 'scripts/release-entry.py', '.github/workflows/release-collect.yml', '.github/workflows/release-publish.yml')


def git(*args, root=None, data=None, env=None):
    return subprocess.check_output(['git', *args], cwd=root or release.ROOT, input=data, env=env).decode().strip()


def blob(commit, path, root=None):
    return subprocess.check_output(['git', 'show', f'{commit}:{path}'], cwd=root or release.ROOT)


def hour_of(batch):
    release.require(isinstance(batch, str) and re.fullmatch(r'[0-9]{10}', batch), 'Hourly batch must be YYYYMMDDHH')
    datetime.datetime.strptime(batch, '%Y%m%d%H')
    return batch


def build_numbers(batch):
    hour_of(batch)
    return int(batch[2:]), f'{int(batch[2:6])}.{int(batch[6:8])}.{int(batch[8:])}'


def version_path(app):
    return f'apps/{app}/' + ('Cargo.toml' if app in RUST else 'app.json' if app == 'mobile' else 'package.json')


def hourly_version(app, base, batch, root=None):
    text = blob(base, version_path(app), root).decode()
    if app in RUST:
        current = re.search(r'(?m)^version\s*=\s*"([^"]+)"', text)[1]
    else:
        data = json.loads(text)
        current = data['expo']['extra']['releaseVersion'] if app == 'mobile' else data['version']
    result = current.split('-', 1)[0] + '-hourly.' + hour_of(batch)
    release.version(result)
    return result


def generated(app, base, batch, pins=None, root=None):
    release.require(app in release.APPS, 'unknown Hourly application')
    pins = pins or {}
    release.require(set(pins) == set(release.DEPENDENCIES[app]), 'Hourly dependency set differs')
    for dependency, pin in pins.items():
        release.validate_pin(dependency, pin)
    value = hourly_version(app, base, batch, root)
    path = version_path(app)
    text = blob(base, path, root).decode()
    result = {}
    if app in RUST:
        text, count = re.subn(r'(?m)^(version\s*=\s*)"[^"]+"', lambda m: m[1] + json.dumps(value), text, count=1)
        release.require(count == 1, 'missing application version')
        blocks = blob(base, 'Cargo.lock', root).split(b'[[package]]')
        count = 0
        for index, block in enumerate(blocks):
            if re.search(rb'(?m)^name = "' + RUST[app].encode() + rb'"$', block):
                release.require(not re.search(rb'(?m)^(source|checksum)\s*=', block), 'local package cannot be a registry package')
                blocks[index] = re.sub(rb'(?m)^version = "[^"]+"$', b'version = ' + json.dumps(value).encode(), block, count=1)
                count += 1
        release.require(count == 1, 'Cargo.lock must contain selected local package once')
        result['Cargo.lock'] = b'[[package]]'.join(blocks)
    else:
        data = json.loads(text)
        if app == 'mobile':
            data['expo']['extra']['releaseVersion'] = value
            code, ios = build_numbers(batch)
            data['expo']['android']['versionCode'] = code
            data['expo']['ios']['buildNumber'] = ios
        else:
            data['version'] = value
        text = json.dumps(data, indent=2, ensure_ascii=False) + '\n'
    result[path] = text.encode()
    if pins:
        result[f'releases/{app}.json'] = (json.dumps(pins, sort_keys=True, indent=2) + '\n').encode()
    return value, result


def expected_tree(app, base, batch, pins=None, root=None):
    _, content = generated(app, base, batch, pins, root)
    with tempfile.TemporaryDirectory() as temporary:
        env = dict(os.environ, GIT_INDEX_FILE=str(Path(temporary) / 'index'))
        git('read-tree', base, env=env, root=root)
        for path, data in content.items():
            oid = git('hash-object', '-w', '--stdin', data=data, root=root)
            git('update-index', '--add', '--cacheinfo', '100644', oid, path, env=env, root=root)
        return git('write-tree', env=env, root=root)


def verify(source, app, root=None):
    parts = git('rev-list', '--parents', '-n', '1', source, root=root).split()
    release.require(len(parts) == 2 and parts[0] == source, 'Hourly source must be one ordinary commit')
    base = parts[1]
    match = re.fullmatch(r'chore\(release\): ' + app + r' hourly ([0-9]{10})', git('show', '-s', '--format=%B', source, root=root))
    release.require(match, 'Hourly application/message mismatch')
    release.require(git('show', '-s', '--format=%ae%n%ce', source, root=root).splitlines() == [BOT, BOT], 'unexpected Hourly author')
    pins = json.loads(blob(source, f'releases/{app}.json', root)) if release.DEPENDENCIES[app] else {}
    release.require(expected_tree(app, base, match[1], pins, root) == git('rev-parse', source + '^{tree}', root=root), 'Hourly source differs from exact selected version and pins')
    return base, match[1]


def source_context(source, app, root=None):
    root = root or release.ROOT
    release.require(app in release.APPS and isinstance(source, str) and re.fullmatch(release.COMMIT, source), 'invalid release source')
    if subprocess.run(['git', 'merge-base', '--is-ancestor', source, 'origin/main'], cwd=root).returncode == 0:
        return {'ci_source_commit': source}
    base, _ = verify(source, app, root)
    release.require(subprocess.run(['git', 'merge-base', '--is-ancestor', base, 'origin/main'], cwd=root).returncode == 0, 'Hourly parent must be on main')
    return {'ci_source_commit': base}


def input_digest(app, source, root=None):
    patterns = (*INPUTS[app], *COMMON, '.github/workflows/' + release.WORKFLOWS[app])
    if app in ('core', 'app', 'www'):
        patterns += ('scripts/release-dispatch.py', '.github/workflows/release-deploy.yml')
    if app == 'cli':
        patterns += ('.github/workflows/release-native.yml', '.github/workflows/release-compose.yml', '.github/workflows/npm-publish.yml', 'scripts/publish-npm.mjs')
    if app == 'desktop':
        patterns += ('.github/workflows/release-desktop.yml',)
    entries = git('ls-tree', '-r', source, root=root).splitlines()
    selected = []
    for line in entries:
        header, path = line.split('\t', 1)
        if any(fnmatch.fnmatchcase(path, pattern) for pattern in patterns):
            selected.append(header + '\t' + path)
    release.require(selected, 'application has no tracked build inputs')
    return hashlib.sha256('\n'.join(selected).encode()).hexdigest()


def api(path, method='GET', data=None, missing=False):
    args = ['gh', 'api', '--method', method, f'repos/{REPO}/{path}']
    if data is not None:
        args += ['--input', '-']
    result = subprocess.run(args, input=json.dumps(data) if data is not None else None, capture_output=True, text=True)
    if missing and result.returncode and '(HTTP 404)' in result.stderr:
        return None
    release.require(result.returncode == 0, f'GitHub {method} request failed: {path}')
    return json.loads(result.stdout) if result.stdout.strip() else None


def green_main():
    runs = api('actions/workflows/ci.yml/runs?branch=main&event=push&status=success&per_page=100')['workflow_runs']
    green = {r['head_sha'] for r in runs if r['head_branch'] == 'main' and r['path'] == '.github/workflows/ci.yml'
             and r['repository']['full_name'] == REPO and r['head_repository']['full_name'] == REPO}
    required = {'.github/workflows/' + path for path in release.WORKFLOWS.values()}
    for source in git('rev-list', '--first-parent', '-n', '200', 'origin/main').splitlines():
        if source not in green:
            continue
        paths = set(git('ls-tree', '-r', '--name-only', source, '--', '.github/workflows').splitlines())
        if required <= paths and b'COORDINATOR_SCHEMA = 2' in blob(source, 'scripts/hourly.py'):
            return source
    raise ValueError('no successful main push CI source contains the independent release workflows')


def tag_commit(tag):
    ref = api('git/ref/tags/' + tag, missing=True)
    if ref is None:
        return None
    item = ref['object']
    for _ in range(8):
        if item['type'] == 'commit':
            return item['sha']
        release.require(item['type'] == 'tag', 'release tag is not a commit')
        item = api('git/tags/' + item['sha'])['object']
    raise ValueError('tag nesting is too deep')


def release_runs(app, tag, source):
    runs = api(f'actions/workflows/{release.WORKFLOWS[app]}/runs?branch={tag}&head_sha={source}&per_page=100')['workflow_runs']
    return [r for r in runs if r['head_sha'] == source and r['head_branch'] == tag and r['repository']['full_name'] == REPO
            and r.get('display_title', '').endswith('/ publish')]


def ensure_release_run(app, tag, source):
    runs = release_runs(app, tag, source)
    if not runs:
        runs = reconcile(lambda: api(f'actions/workflows/{release.WORKFLOWS[app]}/dispatches', 'POST',
                                    {'ref': tag, 'inputs': {'tag': tag, 'publish': 'true'}}),
                         lambda: release_runs(app, tag, source), 'release dispatch')
    return runs


def reconcile(write, read, description):
    try:
        write()
    except (ValueError, subprocess.SubprocessError):
        pass
    for _ in range(18):
        result = read()
        if result:
            return result
        time.sleep(5)
    raise ValueError(description + ' result is unknown; reconcile before retrying')


def version_key(value):
    channel = release.version(value)
    base, _, pre = value.partition('-')
    return (*map(int, base.split('.')), {'hourly': 0, 'rc': 1, 'stable': 2}[channel], int(pre.split('.')[-1]) if pre else 0)


def pending_attempt(app, base, pins):
    """Do not overlap unchanged builds or rebuild a repeatedly failing source every hour."""
    tags = sorted(git('tag', '--list', app + '-v*-hourly.*').splitlines(),
                  key=lambda tag: version_key(release.parse_tag(tag)[1]), reverse=True)
    failures = 0
    for tag in tags:
        source = git('rev-parse', tag + '^{commit}')
        try:
            parent, _ = verify(source, app)
        except ValueError:
            continue
        if input_digest(app, parent) != input_digest(app, base):
            break
        previous_pins = json.loads(blob(source, f'releases/{app}.json')) if release.DEPENDENCIES[app] else {}
        if previous_pins != pins:
            break
        runs = release_runs(app, tag, source)
        if not runs:
            ensure_release_run(app, tag, source)
            return {'status': 'dispatched_existing_tag', 'tag': tag}
        if any(run['status'] != 'completed' for run in runs):
            return {'status': 'in_progress', 'tag': tag}
        if any(run['conclusion'] == 'success' for run in runs):
            return {'status': 'waiting_for_manifest', 'tag': tag}
        failures += 1
        if failures >= 2:
            return {'status': 'failed_source_requires_fix', 'tag': tag}
    return None


def run_hourly(now):
    release.require(os.environ.get('GH_TOKEN') and os.environ.get('GITHUB_REPOSITORY') == REPO
                    and os.environ.get('GITHUB_REF') == 'refs/heads/main', 'coordinator runs only from main in Actions')
    batch = now.astimezone(datetime.timezone.utc).strftime('%Y%m%d%H')
    git('fetch', '--quiet', 'origin', 'main', '--tags')
    base = green_main()
    pages = json.loads(gitless_gh('api', '--paginate', '--slurp', f'repos/{REPO}/releases?per_page=100'))
    records = [r for page in pages for r in page if not r['draft'] and 'release.json' in {a['name'] for a in r['assets']}]
    completed = {}
    for app in release.APPS:
        candidates = []
        for record in records:
            try:
                kind, value, channel = release.parse_tag(record['tag_name'])
            except ValueError:
                continue
            if kind == app and channel == 'hourly':
                candidates.append((version_key(value), record['tag_name']))
        if candidates:
            info, _, sha = release.published(max(candidates)[1])
            completed[app] = (info, sha)
    outcomes, ready = {}, {}
    for app in release.APPS:
        if any(name not in ready for name in release.DEPENDENCIES[app]):
            outcomes[app] = {'status': 'waiting_for_dependencies'}
            continue
        pins = {name: {key: ready[name][0][key] for key in ('tag', 'version', 'source_commit')} | {'manifest_sha256': ready[name][1]}
                for name in release.DEPENDENCIES[app]}
        previous = completed.get(app)
        if previous:
            old_base = source_context(previous[0]['source_commit'], app)['ci_source_commit']
            release.require(subprocess.run(['git', 'merge-base', '--is-ancestor', old_base, base], cwd=release.ROOT).returncode == 0,
                            'coordinator will not publish an older main source')
            if input_digest(app, old_base) == input_digest(app, base) and previous[0]['dependencies'] == pins:
                ready[app] = previous
                outcomes[app] = {'status': 'unchanged', 'tag': previous[0]['tag']}
                continue
        value = hourly_version(app, base, batch)
        tag = release.release_tag_for(app, value)
        source = tag_commit(tag)
        if source is None:
            pending = pending_attempt(app, base, pins)
            if pending:
                outcomes[app] = pending
                continue
            tree = expected_tree(app, base, batch, pins)
            env = dict(os.environ, GIT_AUTHOR_NAME='github-actions[bot]', GIT_AUTHOR_EMAIL=BOT,
                       GIT_COMMITTER_NAME='github-actions[bot]', GIT_COMMITTER_EMAIL=BOT)
            source = git('commit-tree', tree, '-p', base, '-m', f'chore(release): {app} hourly {batch}', env=env)
            release.require(verify(source, app) == (base, batch), 'generated source failed verification')
            reconcile(lambda: git('push', 'origin', source + ':refs/tags/' + tag), lambda: tag_commit(tag), 'tag creation')
            release.require(tag_commit(tag) == source, 'tag points to another source')
        else:
            try:
                existing_base, _ = verify(source, app)
            except ValueError:
                outcomes[app] = {'status': 'hour_occupied', 'tag': tag}
                continue
            if existing_base != base:
                outcomes[app] = {'status': 'hour_occupied', 'tag': tag}
                continue
        runs = ensure_release_run(app, tag, source)
        outcomes[app] = {'status': 'dispatched', 'tag': tag, 'runs': [r['html_url'] for r in runs]}
    print(json.dumps({'batch': batch, 'main_commit': base, 'applications': outcomes}, indent=2))
    return outcomes


def gitless_gh(*args):
    return subprocess.check_output(['gh', *args], text=True)


def self_test():
    import unittest.mock as mock
    app = 'cli'
    pin = lambda name: {'tag': name + '-v0.0.1', 'version': '0.0.1', 'source_commit': 'a' * 40, 'manifest_sha256': 'b' * 64}
    pins = {'core': pin('core'), 'app': pin('app')}
    source = git('rev-parse', 'HEAD')
    for app in release.APPS:
        selected = pins if app == 'cli' else {'cli': pin('cli')} if app == 'desktop' else {}
        value, changes = generated(app, source, '2026111409', selected)
        assert value == '0.0.1-hourly.2026111409'
        assert version_path(app) in changes
        for other in release.APPS:
            if other != app:
                assert version_path(other) not in changes
        if app in RUST:
            before = blob(source, 'Cargo.lock').decode().split('[[package]]')
            after = changes['Cargo.lock'].decode().split('[[package]]')
            modified = [a for a, b in zip(before, after) if a != b]
            assert len(modified) == 1 and f'name = "{RUST[app]}"' in modified[0]
        if app == 'mobile':
            mobile = json.loads(changes[version_path(app)])['expo']
            assert mobile['android']['versionCode'] == 26111409 and mobile['ios']['buildNumber'] == '2611.14.9'
    assert input_digest('core', source) == input_digest('core', source)
    assert release.DEPENDENCIES == {'core': (), 'app': (), 'cli': ('core', 'app'), 'desktop': ('cli',), 'mobile': (), 'www': ()}
    print('Independent Hourly self-test passed: selected version only, exact pins and per-train build numbers')


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--self-test', action='store_true')
    args = parser.parse_args()
    if args.self_test:
        self_test()
    else:
        run_hourly(datetime.datetime.now(datetime.timezone.utc))


if __name__ == '__main__':
    main()
