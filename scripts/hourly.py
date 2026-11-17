#!/usr/bin/env python3
"""Release changed applications and wake exact dependency consumers independently."""

import sys
sys.dont_write_bytecode = True
import release_context
import release_format
import release_publisher
import release_source
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


COORDINATOR_SCHEMA = 2
REPO = release_format.GITHUB_REPO
INPUTS = {
    'core': ('apps/core/*', 'crates/protocol/*', 'crates/control/*', 'crates/bridge/*', 'crates/secure/*', 'crates/client/*', 'crates/delivery/*', 'Cargo.toml', 'Cargo.lock', '.cargo/*', 'config/*', 'rust-toolchain*', 'scripts/sign-release.sh', 'scripts/windows-signing.mjs', 'scripts/private-control.mjs', 'scripts/release-rustflags.py', '.github/workflows/release-native.yml'),
    'app': ('apps/app/*', 'packages/client/*', 'packages/theme/*', 'crates/secure/*', 'Cargo.toml', 'Cargo.lock', 'rust-toolchain*', 'pnpm*', 'package.json', 'scripts/build-web*', 'scripts/build-inputs.py'),
    'cli': ('apps/cli/*', 'crates/control/*', 'crates/client/*', 'crates/bridge/*', 'crates/secure/*', 'crates/protocol/*', 'Cargo.toml', 'Cargo.lock', '.cargo/*', 'rust-toolchain*', 'install.sh', 'scripts/install*', 'scripts/package-*', 'scripts/offline-docs*', 'packages/docs/*', 'scripts/release-contract.mjs', 'scripts/release-metadata.mjs', 'scripts/windows-signing.mjs', 'scripts/private-control.mjs'),
    'desktop': ('apps/desktop/*', 'crates/client/*', 'crates/secure/*', 'crates/bridge/*', 'crates/control/*', 'crates/protocol/*', 'Cargo.toml', 'Cargo.lock', '.cargo/*', 'scripts/build-desktop.mjs', 'scripts/verify-desktop.mjs', 'scripts/release-contract.mjs', 'scripts/release-metadata.mjs', 'scripts/sealed-runtime.mjs', 'scripts/private-control.mjs', 'scripts/zip-directory.py', 'scripts/windows-signing.mjs', 'scripts/import-mac-identity.sh', '.github/actions/native-desktop/*'),
    'mobile': ('apps/mobile/*', 'packages/client/*', 'packages/theme/*', 'packages/secure-native/*', 'crates/secure/*', 'crates/secure-ffi/*', 'Cargo.toml', 'Cargo.lock', 'rust-toolchain*', 'pnpm*', 'package.json', 'scripts/build-mobile*', 'scripts/build-ios*', 'scripts/verify-ios*', '.github/workflows/release-apps.yml', '.github/workflows/release-ios.yml'),
    'www': ('apps/www/*', 'packages/docs/*', 'packages/theme/*', 'pnpm*', 'package.json'),
}
COMMON = ('scripts/release_format.py', 'scripts/release_context.py', 'scripts/release_source.py', 'scripts/release_archive.py', 'scripts/release_inventory.py', 'scripts/release_github.py', 'scripts/release_publisher.py', 'scripts/release_assembly.py', 'scripts/release.py', 'scripts/release-entry.py', '.github/workflows/release-collect.yml', '.github/workflows/release-publish.yml')

















def input_digest(app, source, root=None):
    patterns = (*INPUTS[app], *COMMON, '.github/workflows/' + release_format.WORKFLOWS[app])
    if app in ('core', 'app', 'www'):
        patterns += ('scripts/release-dispatch.py', '.github/workflows/release-deploy.yml')
    if app == 'cli':
        patterns += ('.github/workflows/release-native.yml', '.github/workflows/release-compose.yml', '.github/workflows/npm-publish.yml', 'scripts/publish-npm.mjs')
    if app == 'desktop':
        patterns += ('.github/workflows/release-desktop.yml',)
    entries = release_context.git('ls-tree', '-r', source, root=root).splitlines()
    selected = []
    for line in entries:
        header, path = line.split('\t', 1)
        if any(fnmatch.fnmatchcase(path, pattern) for pattern in patterns):
            selected.append(header + '\t' + path)
    release_format.require(selected, 'application has no tracked build inputs')
    return hashlib.sha256('\n'.join(selected).encode()).hexdigest()


def api(path, method='GET', data=None, missing=False):
    args = ['gh', 'api', '--method', method, f'repos/{REPO}/{path}']
    if data is not None:
        args += ['--input', '-']
    result = subprocess.run(args, input=json.dumps(data) if data is not None else None, capture_output=True, text=True)
    if missing and result.returncode and '(HTTP 404)' in result.stderr:
        return None
    release_format.require(result.returncode == 0, f'GitHub {method} request failed: {path}')
    return json.loads(result.stdout) if result.stdout.strip() else None


def green_main():
    runs = api('actions/workflows/ci.yml/runs?branch=main&event=push&status=success&per_page=100')['workflow_runs']
    green = {r['head_sha'] for r in runs if r['head_branch'] == 'main' and r['path'] == '.github/workflows/ci.yml'
             and r['repository']['full_name'] == REPO and r['head_repository']['full_name'] == REPO}
    required = {'.github/workflows/' + path for path in release_format.WORKFLOWS.values()}
    for source in release_context.git('rev-list', '--first-parent', '-n', '200', 'origin/main').splitlines():
        if source not in green:
            continue
        paths = set(release_context.git('ls-tree', '-r', '--name-only', source, '--', '.github/workflows').splitlines())
        if required <= paths and b'COORDINATOR_SCHEMA = 2' in release_context.blob(source, 'scripts/hourly.py'):
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
        release_format.require(item['type'] == 'tag', 'release tag is not a commit')
        item = api('git/tags/' + item['sha'])['object']
    raise ValueError('tag nesting is too deep')


def release_runs(app, tag, source):
    runs = api(f'actions/workflows/{release_format.WORKFLOWS[app]}/runs?branch={tag}&head_sha={source}&per_page=100')['workflow_runs']
    return [r for r in runs if r['head_sha'] == source and r['head_branch'] == tag and r['repository']['full_name'] == REPO
            and r.get('display_title', '').endswith('/ publish')]


def ensure_release_run(app, tag, source):
    runs = release_runs(app, tag, source)
    if not runs:
        runs = reconcile(lambda: api(f'actions/workflows/{release_format.WORKFLOWS[app]}/dispatches', 'POST',
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
    channel = release_format.version(value)
    base, _, pre = value.partition('-')
    return (*map(int, base.split('.')), {'hourly': 0, 'rc': 1, 'stable': 2}[channel], int(pre.split('.')[-1]) if pre else 0)


def pending_attempt(app, base, pins):
    """Do not overlap unchanged builds or rebuild a repeatedly failing source every hour."""
    tags = sorted(release_context.git('tag', '--list', app + '-v*-hourly.*').splitlines(),
                  key=lambda tag: version_key(release_format.parse_tag(tag)[1]), reverse=True)
    failures = 0
    for tag in tags:
        source = release_context.git('rev-parse', tag + '^{commit}')
        try:
            parent, _ = release_source.verify(source, app)
        except ValueError:
            continue
        if input_digest(app, parent) != input_digest(app, base):
            break
        previous_pins = json.loads(release_context.blob(source, f'releases/{app}.json')) if release_format.DEPENDENCIES[app] else {}
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
    release_format.require(os.environ.get('GH_TOKEN') and os.environ.get('GITHUB_REPOSITORY') == REPO
                    and os.environ.get('GITHUB_REF') == 'refs/heads/main', 'coordinator runs only from main in Actions')
    batch = now.astimezone(datetime.timezone.utc).strftime('%Y%m%d%H')
    release_context.git('fetch', '--quiet', 'origin', 'main', '--tags')
    base = green_main()
    pages = json.loads(gitless_gh('api', '--paginate', '--slurp', f'repos/{REPO}/releases?per_page=100'))
    records = [r for page in pages for r in page if not r['draft'] and 'release.json' in {a['name'] for a in r['assets']}]
    completed = {}
    for app in release_format.APPS:
        candidates = []
        for record in records:
            try:
                kind, value, channel = release_format.parse_tag(record['tag_name'])
            except ValueError:
                continue
            if kind == app and channel == 'hourly':
                candidates.append((version_key(value), record['tag_name']))
        if candidates:
            info, _, sha = release_publisher.published(max(candidates)[1])
            completed[app] = (info, sha)
    outcomes, ready = {}, {}
    for app in release_format.APPS:
        if any(name not in ready for name in release_format.DEPENDENCIES[app]):
            outcomes[app] = {'status': 'waiting_for_dependencies'}
            continue
        pins = {name: {key: ready[name][0][key] for key in ('tag', 'version', 'source_commit')} | {'manifest_sha256': ready[name][1]}
                for name in release_format.DEPENDENCIES[app]}
        previous = completed.get(app)
        if previous:
            old_base = release_source.validate_source(previous[0]['source_commit'], app)['ci_source_commit']
            release_format.require(subprocess.run(['git', 'merge-base', '--is-ancestor', old_base, base], cwd=release_context.ROOT).returncode == 0,
                            'coordinator will not publish an older main source')
            if input_digest(app, old_base) == input_digest(app, base) and previous[0]['dependencies'] == pins:
                ready[app] = previous
                outcomes[app] = {'status': 'unchanged', 'tag': previous[0]['tag']}
                continue
        value = release_source.hourly_version(app, base, batch)
        tag = release_format.release_tag_for(app, value)
        source = tag_commit(tag)
        if source is None:
            pending = pending_attempt(app, base, pins)
            if pending:
                outcomes[app] = pending
                continue
            tree = release_source.expected_tree(app, base, batch, pins)
            env = dict(os.environ, GIT_AUTHOR_NAME='github-actions[bot]', GIT_AUTHOR_EMAIL=release_source.BOT,
                       GIT_COMMITTER_NAME='github-actions[bot]', GIT_COMMITTER_EMAIL=release_source.BOT)
            source = release_context.git('commit-tree', tree, '-p', base, '-m', f'chore(release): {app} hourly {batch}', env=env)
            release_format.require(release_source.verify(source, app) == (base, batch), 'generated source failed verification')
            reconcile(lambda: release_context.git('push', 'origin', source + ':refs/tags/' + tag), lambda: tag_commit(tag), 'tag creation')
            release_format.require(tag_commit(tag) == source, 'tag points to another source')
        else:
            try:
                existing_base, _ = release_source.verify(source, app)
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
    source = release_context.git('rev-parse', 'HEAD')
    for app in release_format.APPS:
        selected = pins if app == 'cli' else {'cli': pin('cli')} if app == 'desktop' else {}
        value, changes = release_source.generated(app, source, '2026111409', selected)
        assert value == '0.0.1-hourly.2026111409'
        assert release_source.version_path(app) in changes
        for other in release_format.APPS:
            if other != app:
                assert release_source.version_path(other) not in changes
        if app in release_source.RUST:
            before = release_context.blob(source, 'Cargo.lock').decode().split('[[package]]')
            after = changes['Cargo.lock'].decode().split('[[package]]')
            modified = [a for a, b in zip(before, after) if a != b]
            assert len(modified) == 1 and f'name = "{release_source.RUST[app]}"' in modified[0]
        if app == 'mobile':
            mobile = json.loads(changes[release_source.version_path(app)])['expo']
            assert mobile['android']['versionCode'] == 26111409 and mobile['ios']['buildNumber'] == '2611.14.9'
    assert input_digest('core', source) == input_digest('core', source)
    assert release_format.DEPENDENCIES == {'core': (), 'app': (), 'cli': ('core', 'app'), 'desktop': ('cli',), 'mobile': (), 'www': ()}
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
