#!/usr/bin/env python3
"""Reconcile hourly Core and daily client releases from verified main sources."""

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
import subprocess
import time
from zoneinfo import ZoneInfo


COORDINATOR_SCHEMA = 2
REPO = release_format.GITHUB_REPO
CLIENT_APPS = ('app', 'cli', 'desktop', 'mobile')
CLIENT_TIME_ZONE = 'America/Los_Angeles'
CLIENT_RELEASE_HOUR = 18
PENDING = {'dispatched', 'dispatched_existing_tag', 'in_progress', 'waiting_for_manifest'}
INPUTS = {
    'core': ('apps/core/*', 'crates/protocol/*', 'crates/control/*', 'crates/bridge/*', 'crates/secure/*', 'crates/client/*', 'crates/delivery/*', 'Cargo.toml', 'Cargo.lock', '.cargo/*', 'config/*', 'rust-toolchain*', 'scripts/sign-release.sh', 'scripts/windows-signing.mjs', 'scripts/private-control.mjs', 'scripts/release-rustflags.py', '.github/workflows/release-native.yml'),
    'app': ('apps/app/*', 'packages/client/*', 'packages/theme/*', 'crates/secure/*', 'Cargo.toml', 'Cargo.lock', 'rust-toolchain*', 'pnpm*', 'package.json', 'scripts/build-web*', 'scripts/check-web.sh', 'scripts/build-inputs.py'),
    'cli': ('apps/cli/*', 'apps/core/extensions/generated/pi.ts', 'apps/core/extensions/generated/opencode.ts', 'crates/control/*', 'crates/client/*', 'crates/bridge/*', 'crates/native-transport/*', 'crates/secure/*', 'crates/protocol/*', 'Cargo.toml', 'Cargo.lock', '.cargo/*', 'rust-toolchain*', 'install.sh', 'scripts/install*', 'scripts/package-*', 'scripts/offline-docs*', 'packages/docs/*', 'scripts/release-contract.mjs', 'scripts/release-metadata.mjs', 'scripts/windows-signing.mjs', 'scripts/private-control.mjs'),
    'desktop': ('apps/desktop/*', 'crates/client/*', 'crates/native-transport/*', 'crates/secure/*', 'crates/bridge/*', 'crates/control/*', 'crates/protocol/*', 'Cargo.toml', 'Cargo.lock', '.cargo/*', 'scripts/build-desktop.mjs', 'scripts/verify-desktop.mjs', 'scripts/release-contract.mjs', 'scripts/release-metadata.mjs', 'scripts/sealed-runtime.mjs', 'scripts/private-control.mjs', 'scripts/zip-directory.py', 'scripts/windows-signing.mjs', 'scripts/import-mac-identity.sh', '.github/actions/native-desktop/*'),
    'mobile': ('apps/mobile/*', 'packages/client/*', 'packages/theme/*', 'packages/secure-native/*', 'crates/secure/*', 'crates/secure-ffi/*', 'crates/native-transport/*', 'crates/protocol/*', 'Cargo.toml', 'Cargo.lock', 'rust-toolchain*', 'pnpm*', 'package.json', 'scripts/build-mobile*', 'scripts/build-ios*', 'scripts/verify-ios*', '.github/workflows/release-apps.yml', '.github/workflows/release-ios.yml'),
    'www': ('apps/www/*', 'packages/docs/*', 'packages/theme/*', 'pnpm*', 'package.json'),
}
COMMON = ('scripts/release_component_identity.py', 'scripts/release_format.py', 'scripts/release_context.py', 'scripts/release_source.py', 'scripts/release_archive.py', 'scripts/release_inventory.py', 'scripts/release_github.py', 'scripts/release_publisher.py', 'scripts/release_assembly.py', 'scripts/release.py', 'scripts/release-entry.py', '.github/workflows/release-collect.yml', '.github/workflows/release-publish.yml', '.github/actions/setup-js/action.yml')

def input_digest(app, source, root=None):
    patterns = (*INPUTS[app], *COMMON, '.github/workflows/' + release_format.WORKFLOWS[app])
    if app != 'www':
        patterns += ('.github/actions/setup-rust/action.yml',)
    if app == 'app':
        patterns += ('.github/actions/setup-web-crypto/action.yml', 'scripts/wasm-bindgen-version.py')
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


def green_main(cutoff=None):
    query = 'actions/workflows/ci.yml/runs?branch=main&event=push&status=success&per_page=100'
    if cutoff is not None:
        query += '&created=%3C%3D' + cutoff.strftime('%Y-%m-%dT%H:%M:%SZ')
    runs = api(query)['workflow_runs']
    green = {r['head_sha'] for r in runs if r['head_branch'] == 'main' and r['path'] == '.github/workflows/ci.yml'
             and r['repository']['full_name'] == REPO and r['head_repository']['full_name'] == REPO
             and (cutoff is None or datetime.datetime.fromisoformat(r['updated_at']) <= cutoff)}
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


def is_ancestor(ancestor, source):
    return subprocess.run(['git', 'merge-base', '--is-ancestor', ancestor, source], cwd=release_context.ROOT).returncode == 0


def pending_attempt(app, base, batch, pins):
    """Reuse pending builds and cap retries for the same release inputs."""
    tags = sorted(release_context.git('tag', '--list', app + '-v*-hourly.*').splitlines(),
                  key=lambda tag: version_key(release_format.parse_tag(tag)[1]), reverse=True)
    failures = 0
    for tag in tags:
        source = release_context.git('rev-parse', tag + '^{commit}')
        try:
            parent, attempt_batch = release_source.verify(source, app)
        except ValueError:
            continue
        if attempt_batch > batch or not is_ancestor(parent, base):
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


def require_coordinator_context():
    release_format.require(os.environ.get('GH_TOKEN') and os.environ.get('GITHUB_REPOSITORY') == REPO
                    and os.environ.get('GITHUB_REF') == 'refs/heads/main', 'coordinator runs only from main in Actions')


def complete_manifest(record):
    if record and not record['draft']:
        return next((asset for asset in record['assets'] if asset['name'] == 'release.json'), None)
    return None


def latest_release_tags(apps, core_cutoff=None):
    pages = json.loads(gitless_gh('api', '--paginate', '--slurp', f'repos/{REPO}/releases?per_page=100'))
    newest = {}
    for record in (item for page in pages for item in page):
        try:
            app, value, channel = release_format.parse_tag(record['tag_name'])
        except ValueError:
            continue
        if app not in apps or channel != 'hourly':
            continue
        manifest = complete_manifest(record)
        if not manifest or (app == 'core' and core_cutoff is not None
                            and datetime.datetime.fromisoformat(manifest['created_at']) > core_cutoff):
            continue
        candidate = (version_key(value), record['tag_name'])
        if app not in newest or candidate > newest[app]:
            newest[app] = candidate
    return {app: tag for app, (_, tag) in newest.items()}


def published_pin(tag):
    info, _, sha = release_publisher.published(tag)
    return {key: info[key] for key in ('tag', 'version', 'source_commit')} | {'manifest_sha256': sha}


def reconcile_app(app, base, batch, pins, previous_tag):
    release_format.require(app == 'core' or app in CLIENT_APPS, 'application has no scheduled release')
    value = release_source.hourly_version(app, base, batch)
    tag = release_format.release_tag_for(app, value)
    if previous_tag:
        previous, _, _ = release_publisher.published(previous_tag)
        old_base = release_source.validate_source(previous['source_commit'], app)['ci_source_commit']
        release_format.require(version_key(previous['version']) <= version_key(value) and is_ancestor(old_base, base),
                               'coordinator will not publish an older main source or version')
        if input_digest(app, old_base) == input_digest(app, base) and previous['dependencies'] == pins:
            return {'status': 'unchanged', 'tag': previous_tag}
    source = tag_commit(tag)
    if source is None:
        pending = pending_attempt(app, base, batch, pins)
        if pending:
            return pending
        tree = release_source.expected_tree(app, base, batch, pins)
        env = dict(os.environ, GIT_AUTHOR_NAME='github-actions[bot]', GIT_AUTHOR_EMAIL=release_source.BOT,
                   GIT_COMMITTER_NAME='github-actions[bot]', GIT_COMMITTER_EMAIL=release_source.BOT)
        source = release_context.git('commit-tree', tree, '-p', base, '-m', f'chore(release): {app} hourly {batch}', env=env)
        release_format.require(release_source.verify(source, app) == (base, batch), 'generated source failed verification')
        actual = reconcile(lambda: release_context.git('push', 'origin', source + ':refs/tags/' + tag),
                           lambda: tag_commit(tag), 'tag creation')
        release_format.require(actual == source, 'tag points to another source')
    else:
        try:
            identity = release_source.verify(source, app)
        except ValueError:
            return {'status': 'hour_occupied', 'tag': tag}
        if identity != (base, batch) or (pins and json.loads(release_context.blob(source, f'releases/{app}.json')) != pins):
            return {'status': 'hour_occupied', 'tag': tag}
    runs = ensure_release_run(app, tag, source)
    return {'status': 'dispatched', 'tag': tag, 'runs': [r['html_url'] for r in runs]}


def run_hourly(now):
    require_coordinator_context()
    batch = now.astimezone(datetime.timezone.utc).strftime('%Y%m%d%H')
    release_context.git('fetch', '--quiet', 'origin', 'main', '--tags')
    base = green_main()
    previous = latest_release_tags(('core',))
    outcomes = {'core': reconcile_app('core', base, batch, {}, previous.get('core'))}
    print(json.dumps({'batch': batch, 'main_commit': base, 'applications': outcomes}, indent=2))
    return outcomes


def client_cutoff(created_at):
    local = created_at.astimezone(ZoneInfo(CLIENT_TIME_ZONE))
    slot = local.replace(hour=CLIENT_RELEASE_HOUR, minute=0, second=0, microsecond=0)
    if local < slot:
        slot -= datetime.timedelta(days=1)
    return slot.astimezone(datetime.timezone.utc)


def client_source(cutoff, batch):
    # A forced CI rerun can change which runs the API reports as green at the cutoff.
    # Each generated tag records the main parent selected when the batch began.
    sources = set()
    for app in CLIENT_APPS:
        for tag in release_context.git('tag', '--list', f'{app}-v*-hourly.*').splitlines():
            _, version, _ = release_format.parse_tag(tag)
            tag_batch = version.rsplit('.', 1)[1]
            if tag_batch < batch:
                continue
            source = release_context.git('rev-parse', tag + '^{commit}')
            try:
                parent, generated_batch = release_source.verify(source, app)
            except ValueError:
                continue
            if generated_batch != tag_batch or not is_ancestor(parent, 'origin/main'):
                continue
            release_format.require(tag_batch == batch, 'a newer client batch already has a generated tag')
            sources.add(parent)
    release_format.require(len(sources) <= 1, 'daily client tags disagree on the selected main source')
    return sources.pop() if sources else green_main(cutoff)


def run_clients():
    require_coordinator_context()
    release_format.require(os.environ.get('GITHUB_EVENT_NAME') == 'schedule', 'clients require the daily schedule')
    run = api('actions/runs/' + os.environ['GITHUB_RUN_ID'])
    cutoff = client_cutoff(datetime.datetime.fromisoformat(run['created_at']))
    recent = api(f"actions/workflows/{run['workflow_id']}/runs?branch=main&event=schedule&per_page=1")['workflow_runs']
    if recent:
        latest_cutoff = client_cutoff(datetime.datetime.fromisoformat(recent[0]['created_at']))
        release_format.require(latest_cutoff <= cutoff, 'a newer scheduled client batch exists')
    batch = cutoff.strftime('%Y%m%d%H')
    release_context.git('fetch', '--quiet', 'origin', 'main', '--tags')
    base = client_source(cutoff, batch)
    previous = latest_release_tags(('core', *CLIENT_APPS), core_cutoff=cutoff)
    ready, outcomes, active = {}, {}, {}
    try:
        release_format.require('core' in previous, 'no complete Core release at the daily cutoff')
        ready['core'] = published_pin(previous['core'])
    except (ValueError, subprocess.SubprocessError) as error:
        outcomes['core_dependency'] = {'status': 'failed', 'error': str(error)}

    while True:
        for app, (tag, source) in list(active.items()):
            try:
                runs = release_runs(app, tag, source)
                if app not in ready and complete_manifest(api('releases/tags/' + tag, missing=True)):
                    ready[app] = published_pin(tag)
                if any(r['status'] != 'completed' for r in runs):
                    continue
                successful = app in ready and any(r['conclusion'] == 'success' for r in runs)
                status = outcomes[app]['status'] if outcomes[app]['status'] == 'unchanged' else 'released'
                outcomes[app] = {'status': status if successful else 'failed', 'tag': tag,
                                 'runs': [r['html_url'] for r in runs]}
            except (ValueError, subprocess.SubprocessError) as error:
                outcomes[app] = {'status': 'failed', 'tag': tag, 'error': str(error)}
            del active[app]
            print(json.dumps({app: outcomes[app]}), flush=True)

        for app in CLIENT_APPS:
            dependencies = release_format.DEPENDENCIES[app]
            if app in outcomes or any(dependency not in ready for dependency in dependencies):
                continue
            try:
                pins = {dependency: ready[dependency] for dependency in dependencies}
                outcome = reconcile_app(app, base, batch, pins, previous.get(app))
                tag = outcome['tag']
                if outcome['status'] in PENDING:
                    source = tag_commit(tag)
                    release_format.require(source, 'dispatched release tag could not be resolved')
                    active[app] = (tag, source)
                elif outcome['status'] == 'unchanged':
                    ready[app] = published_pin(tag)
                    active[app] = (tag, ready[app]['source_commit'])
                outcomes[app] = outcome
            except (ValueError, subprocess.SubprocessError) as error:
                outcomes[app] = {'status': 'failed', 'error': str(error)}
            print(json.dumps({app: outcomes[app]}), flush=True)
        if not active:
            break
        time.sleep(60)

    for app in CLIENT_APPS:
        if app not in outcomes:
            outcomes[app] = {'status': 'waiting_for_dependencies',
                             'dependencies': [dep for dep in release_format.DEPENDENCIES[app] if dep not in ready]}
    print(json.dumps({'batch': batch, 'main_commit': base, 'applications': outcomes}, indent=2))
    release_format.require(all(outcome['status'] in ('unchanged', 'released') for outcome in outcomes.values()),
                           'daily client release did not complete for every application')
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
    with mock.patch.object(release_context, 'git', return_value='100644 blob ' + 'a' * 40 + '\tCargo.toml\n'):
        unchanged = input_digest('app', source)
    for content in ('a', 'b'):
        listing = '100644 blob ' + 'a' * 40 + '\tCargo.toml\n100644 blob ' + content * 40 + '\tscripts/check-web.sh\n'
        with mock.patch.object(release_context, 'git', return_value=listing):
            selected = input_digest('app', source)
        assert selected != unchanged, 'App build input digest must include Web checks'
        unchanged = selected
    for provider in ('pi', 'opencode'):
        source_path = f'apps/core/extensions/{provider}.ts'
        template = f'apps/core/extensions/generated/{provider}.ts'
        for path, embedded in ((source_path, False), (template, True)):
            listing = f'100644 blob base\tCargo.toml\n100644 blob before\t{path}\n'
            with mock.patch.object(release_context, 'git', return_value=listing):
                before = input_digest('cli', source)
            with mock.patch.object(release_context, 'git', return_value=listing.replace('blob before', 'blob after')):
                assert (input_digest('cli', source) != before) == embedded, path
    for path, consumers in (
        ('.github/actions/setup-js/action.yml', set(release_format.APPS)),
        ('.github/actions/setup-rust/action.yml', set(release_format.APPS) - {'www'}),
        ('.github/actions/setup-web-crypto/action.yml', {'app'}),
        ('scripts/wasm-bindgen-version.py', {'app'}),
    ):
        listing = f'100644 blob base\tpackage.json\n100644 blob base\tCargo.toml\n100644 blob before\t{path}\n'
        with mock.patch.object(release_context, 'git', return_value=listing):
            before = {app: input_digest(app, source) for app in release_format.APPS}
        with mock.patch.object(release_context, 'git', return_value=listing.replace('blob before', 'blob after')):
            changed = {app for app, digest in before.items() if input_digest(app, source) != digest}
        assert changed == consumers, (path, changed)
    assert release_format.DEPENDENCIES == {'core': (), 'app': (), 'cli': ('core', 'app'), 'desktop': ('cli',), 'mobile': (), 'www': ()}
    print('Independent Hourly self-test passed: selected version only, exact pins and per-train build numbers')


def main():
    parser = argparse.ArgumentParser()
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument('--self-test', action='store_true')
    mode.add_argument('--clients', action='store_true')
    args = parser.parse_args()
    if args.self_test:
        self_test()
    elif args.clients:
        run_clients()
    else:
        run_hourly(datetime.datetime.now(datetime.timezone.utc))


if __name__ == '__main__':
    main()
