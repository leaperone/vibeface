#!/usr/bin/env python3
"""Tag one mechanical source per four-hour UTC batch, then reconcile both Nightlies."""
import argparse
import datetime
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import time
from urllib.parse import quote

sys.dont_write_bytecode = True
import release
from nightly_source import (DailyPlan, BOT, PATHS, blob, checkout, generated, git, locked_package_version, metadata,
                            next_build_numbers, next_ios_build_number, nightly_order, recovered_plan,
                            scratch, verify_tree, versions_at)

ROOT = Path(__file__).resolve().parent.parent
REPO = 'leaperone/vibeface'
CI = '.github/workflows/ci.yml'
NIGHTLY = '.github/workflows/nightly.yml'
RELEASE = '.github/workflows/release.yml'
PRODUCT_APPS = release.PRODUCT_APPS
RELEASES = release.RELEASES
PRODUCER = 'collect'
PART_PUBLISHERS = (
    'publish-app',
    'publish-native-aarch64-apple-darwin',
    'publish-native-x86_64-unknown-linux-gnu',
    'publish-native-aarch64-unknown-linux-gnu',
    'publish-native-x86_64-pc-windows-msvc',
    'publish-desktop-aarch64-apple-darwin',
    'publish-desktop-x86_64-unknown-linux-gnu',
    'publish-desktop-aarch64-unknown-linux-gnu',
    'publish-desktop-x86_64-pc-windows-msvc',
    'publish-android', 'publish-ios')
PUBLISHERS = ('release-start', *PART_PUBLISHERS, 'publish-product', 'release-failed', 'publish', 'npm')
require = release.require


def schedule_batch(created_at):
    created = datetime.datetime.fromisoformat(created_at.replace('Z', '+00:00')).astimezone(datetime.timezone.utc)
    return created.replace(hour=created.hour // 4 * 4).strftime('%Y%m%d%H')


def api(path, method='GET', data=None, missing=False):
    environment = dict(os.environ)
    args = ['gh', 'api', '--method', method, 'repos/' + REPO + '/' + path]
    if data is not None:
        args += ['--input', '-']
    result = subprocess.run(args, input=json.dumps(data) if data is not None else None,
                            env=environment, capture_output=True, text=True)
    if missing and result.returncode and '(HTTP 404)' in result.stderr:
        return None
    require(result.returncode == 0, f'GitHub {method} {path} failed; reconcile remote state before retrying')
    return json.loads(result.stdout) if result.stdout.strip() else None


def pages(path, key=None):
    values = []
    for page in range(1, 1001):
        result = api(path + ('&' if '?' in path else '?') + f'per_page=100&page={page}')
        items = result[key] if key else result
        require(isinstance(items, list), 'invalid paginated GitHub response')
        values.extend(items)
        if len(items) < 100:
            return values
    raise ValueError('GitHub pagination limit exceeded')


def reconcile_write(write, read, accepted, description):
    # A failed response can follow a successful write. Only remote facts permit progress.
    try:
        write()
    except (ValueError, subprocess.SubprocessError):
        pass
    for attempt in range(18):
        value = read()
        if accepted(value):
            return value
        if attempt < 17:
            time.sleep(10)
    raise ValueError(description + ' has an unknown or rejected outcome; no write was repeated')


def declared_versions(tags):
    """Versions already claimed by real release tags, per release group."""
    values = {group: [] for group in RELEASES}
    for tag in tags:
        try:
            group, value, _ = release.parse_tag(tag)
        except ValueError:
            continue
        values[group].append(value)
    return values


def next_sequence(prefix, claimed):
    sequences = [int(value[len(prefix):]) for value in claimed
                 if value.startswith(prefix) and re.fullmatch(r'[1-9][0-9]*', value[len(prefix):])]
    return prefix + str(max(sequences, default=0) + 1)


def plan_versions(batch, base, tags, run_id, previous=''):
    day = batch[:8]
    values = versions_at(base)
    for app in ('cli', 'www'):
        if release.version(values[app]) == 'nightly':
            current_day = values[app].split('-nightly.', 1)[1].split('.', 1)[0]
            require(day >= current_day, f'unstarted Nightly batch predates current {app} version; refusing a downgrade')
    claimed = declared_versions(tags)
    product_prefix = values['cli'].split('-', 1)[0] + f'-nightly.{day}.'
    product_ver = next_sequence(product_prefix, claimed['product'] + [values[app] for app in PRODUCT_APPS])
    www_prefix = values['www'].split('-', 1)[0] + f'-nightly.{day}.'
    values['www'] = next_sequence(www_prefix, claimed['www'] + [values['www']])
    for app in PRODUCT_APPS:
        values[app] = product_ver
    code, ios = next_build_numbers(base, previous)
    return DailyPlan(batch, base, values, code, ios, str(run_id), previous)


def matching_runs(workflow, sha, branch, events):
    values = pages('actions/workflows/' + workflow.split('/')[-1] + '/runs?head_sha=' + sha + '&branch=' + quote(branch, safe=''), 'workflow_runs')
    return [run for run in values if run['head_sha'] == sha and run['head_branch'] == branch
            and run['event'] in events and run['path'] == workflow
            and run['repository']['full_name'] == REPO and run['head_repository']['full_name'] == REPO]


def rerun(run, failed_only):
    attempt = run['run_attempt']
    return reconcile_write(lambda: api(f'actions/runs/{run["id"]}/' + ('rerun-failed-jobs' if failed_only else 'rerun'), 'POST'),
                           lambda: api(f'actions/runs/{run["id"]}'),
                           lambda value: value['run_attempt'] > attempt or value['status'] != 'completed', 'original run recovery')


def tag_sha(tag):
    ref = api('git/ref/tags/' + tag, missing=True)
    if not ref:
        return None
    item = ref['object']
    for _ in range(8):
        if item['type'] == 'commit':
            return item['sha']
        require(item['type'] == 'tag', 'release ref is not a commit or annotated tag')
        item = api('git/tags/' + item['sha'])['object']
    raise ValueError('too many nested release tag objects')


def ensure_tag(tag, source):
    existing = tag_sha(tag)
    if existing is None:
        existing = reconcile_write(lambda: git('push', 'origin', source + ':refs/tags/' + tag),
                                   lambda: tag_sha(tag), lambda value: value is not None, 'release tag creation')
    require(existing == source, 'existing immutable tag points to a different SHA')


def release_state(tag):
    return api('releases/tags/' + tag, missing=True)


def release_dispatch_tag(app):
    """Daily Nightly publishes exactly two groups: product v<version> and www www-v<version>."""
    require(app in RELEASES, 'daily Nightly publishes only the product and www release groups')
    return release.identity(app)['tag']


def start_release(app, source):
    workflow, extra_inputs = release.release_workflow(source, app)
    tag = release_dispatch_tag(app)
    ensure_tag(tag, source)
    state = release_state(tag)
    runs = matching_runs(workflow, source, tag, {'push', 'workflow_dispatch'})
    if state and not state['draft']:
        return runs
    if not runs:
        require(state is None, 'draft release has no original run; original bytes cannot be recovered')
        # The release workflow only builds unless publish is requested explicitly.
        inputs = dict(extra_inputs, tag=tag, publish='true')
        runs = reconcile_write(lambda: api('actions/workflows/' + workflow.split('/')[-1] + '/dispatches', 'POST', dict(ref=tag, inputs=inputs)),
                               lambda: matching_runs(workflow, source, tag, {'push', 'workflow_dispatch'}), bool, 'release workflow dispatch')
    return runs


def job_leaf(job):
    return job['name'].split(' / ')[-1]


def original_artifacts(app, run, source):
    """Immutable artifact IDs the publisher jobs must reuse instead of rebuilding."""
    prefixes = ([f'www-release-{source}-'] if app == 'www' else [
        f'static-release-{source}-',
        *(f'native-release-{target}-{source}-' for target in release.TARGETS),
        *(f'desktop-release-{target}-{source}-' for target in release.TARGETS),
        f'android-release-{source}-', f'ios-device-archive-{source}-',
        f'product-release-{source}-'])
    if app == 'product':
        _, _, channel = release.parse_tag(release_dispatch_tag(app))
        if channel == 'nightly':
            prefixes.append(f'npm-package-{source}-')
    available = [artifact for artifact in pages(f'actions/runs/{run["id"]}/artifacts', 'artifacts')
                 if not artifact['expired'] and artifact['size_in_bytes'] > 0
                 and (artifact.get('workflow_run') or {}).get('id') == run['id']
                 and (artifact.get('workflow_run') or {}).get('head_sha') == source]
    identifiers = {}
    for prefix in prefixes:
        matches = [artifact for artifact in available
                   if re.fullmatch(re.escape(prefix) + r'[1-9][0-9]*', artifact['name'])
                   and int(artifact['name'][len(prefix):]) <= run['run_attempt']]
        require(len(matches) == 1 and type(matches[0]['id']) is int and matches[0]['id'] > 0,
                'original producer artifact is unavailable; refusing a rebuild')
        identifiers[prefix] = matches[0]['id']
    return identifiers


def recover_publisher(app, run, source):
    require(app in RELEASES, 'only the product and www release groups can be recovered')
    jobs = pages(f'actions/runs/{run["id"]}/jobs?filter=latest', 'jobs')
    producers = [job for job in jobs if job_leaf(job) == PRODUCER]
    require(len(producers) == 1 and producers[0]['conclusion'] == 'success',
            'the complete original-artifact audit did not succeed; refusing a publisher recovery')
    failed = [job for job in jobs if job['conclusion'] not in ('success', 'skipped')]
    allowed = set(PUBLISHERS if app == 'product' else ('publish',))
    require(failed and all(job['status'] == 'completed' and job_leaf(job) in allowed for job in failed),
            'failure includes a producer or unknown job; only a named original-artifact publisher may be recovered')
    names = {job_leaf(job) for job in failed}
    if app == 'product':
        require('publish' not in names, 'product recovery cannot rerun the independent website publisher')
    else:
        require(names == {'publish'}, 'website recovery must contain only its publisher')
    original_artifacts(app, run, source)
    return rerun(run, True)


def finish_release(app, source, deadline):
    workflow, _ = release.release_workflow(source, app)
    tag = release_dispatch_tag(app)
    restored = set()
    while time.monotonic() < deadline:
        require(tag_sha(tag) == source, 'release tag changed source')
        runs = matching_runs(workflow, source, tag, {'push', 'workflow_dispatch'})
        state = release_state(tag)
        active = [run for run in runs if run['status'] != 'completed']
        if not active:
            require(runs, 'release has no original workflow run')
            require(len(runs) == 1, 'multiple release runs for one tag; reconcile original artifacts manually')
            run = runs[0]
            # A public Release with a failed npm job is not a success; it is a recoverable publisher failure.
            if state and not state['draft'] and run['conclusion'] == 'success':
                require(app != 'product' or npm_complete(run), 'original Nightly npm publisher did not succeed')
                with tempfile.TemporaryDirectory(dir=scratch(), prefix='public-') as temporary:
                    release.verify_public(app, Path(temporary) / 'assets')
                return [dict(id=run['id'], url=run['html_url'], conclusion=run['conclusion'])]
            require(run['id'] not in restored, 'release is not complete after original workflow completion')
            restored.add(run['id'])
            recover_publisher(app, run, source)
        time.sleep(15)
    raise ValueError('release workflow is still pending; rerun the original daily coordinator to reconcile')


def publish_daily(source, deadline, groups=RELEASES):
    receipt = {}; started = []; errors = []
    for app in groups:
        started.append(app)
        try:
            start_release(app, source)
        except Exception as error:
            errors.append(f'{app}: {error}')
            break
    if not errors:
        for app in groups:
            if app not in started:
                continue
            try:
                receipt[app] = finish_release(app, source, deadline)
            except Exception as error:
                errors.append(f'{app}: {error}')
    for app in started:
        workflow, _ = release.release_workflow(source, app)
        tag = release_dispatch_tag(app)
        runs = matching_runs(workflow, source, tag, {'push', 'workflow_dispatch'})
        while any(run['status'] != 'completed' for run in runs) and time.monotonic() < deadline:
            time.sleep(15)
            runs = matching_runs(workflow, source, tag, {'push', 'workflow_dispatch'})
        print(json.dumps(dict(app=app, runs=[dict(id=run['id'], url=run['html_url'], status=run['status'],
                                                conclusion=run['conclusion']) for run in runs])))
        if any(run['status'] != 'completed' for run in runs):
            errors.append(f'{app}: associated runs remain pending')
    require(not errors, '; '.join(errors))
    return receipt


def trusted_nightlies(tags, batch):
    """Either group may be the sole surviving tag of a partially published source."""
    trusted = {}
    for tag in tags:
        try:
            app, value, channel = release.parse_tag(tag)
        except ValueError:
            continue
        if channel != 'nightly':
            continue
        source = git('rev-parse', tag + '^{commit}')
        try:
            require(set(git('ls-tree', '-r', '--name-only', source, '--', *PATHS).splitlines()) == set(PATHS),
                    'Nightly candidate predates the current release layout')
            context = release.validate_source(source, app)
            if app == 'www' and not context['ci_run_id']:
                continue
            values = versions_at(source)
            require(values['cli' if app == 'product' else 'www'] == value, 'Nightly tag version mismatch')
            order = nightly_order(values['cli'])
        except ValueError:
            require(git('show', '-s', '--format=%s', source) != 'chore(release): nightly ' + batch,
                    'existing Nightly batch tag has an invalid immutable source')
            continue
        trusted[source] = (order, tag, source, context)
    return [(tag, source, context) for _, tag, source, context in sorted(trusted.values(), reverse=True)]


def npm_complete(run):
    jobs = pages(f'actions/runs/{run["id"]}/jobs?filter=latest', 'jobs')
    npm = [job for job in jobs if job_leaf(job) == 'npm']
    return len(npm) == 1 and npm[0]['status'] == 'completed' and npm[0]['conclusion'] == 'success'


def completed_source(source):
    """A complete product includes the original npm publisher, not merely a public Release."""
    with checkout(source) as clean:
        previous_root = release.ROOT
        release.ROOT = clean
        try:
            groups = RELEASES if release.validate_source(source, 'product')['ci_run_id'] else ('product',)
            for app in groups:
                workflow, _ = release.release_workflow(source, app)
                tag = release_dispatch_tag(app)
                require(release.parse_tag(tag)[2] == 'nightly', 'recovery only accepts Nightly groups')
                if tag_sha(tag) != source:
                    return False
                state = release_state(tag)
                runs = matching_runs(workflow, source, tag, {'push', 'workflow_dispatch'})
                if not (state and not state['draft'] and len(runs) == 1
                        and runs[0]['status'] == 'completed' and runs[0]['conclusion'] == 'success'):
                    return False
                if app == 'product' and not npm_complete(runs[0]):
                    return False
                with tempfile.TemporaryDirectory(dir=scratch(), prefix='complete-') as temporary:
                    release.verify_public(app, Path(temporary) / 'assets')
            return True
        finally:
            release.ROOT = previous_root


def ensure_source(batch, base, run_id):
    git('fetch', 'origin', 'main', '--tags')
    tags = [line.split('\t')[1][10:] for line in git('ls-remote', '--tags', 'origin').splitlines()
            if not line.endswith('^{}')]
    # Fetch before reading any immutable ref, including a batch stopped after only its WWW tag.
    trusted = trusted_nightlies(tags, batch)
    sources = {source for _, source, context in trusted if context['nightly_batch'] == batch}
    require(len(sources) <= 1, 'Nightly batch has conflicting immutable sources')
    if sources:
        source = sources.pop()
        plan = recovered_plan(source)
        require(plan.base == base and plan.run_id == str(run_id), 'Nightly batch differs from its original schedule source')
        return source, completed_source(source)
    # Finish the previous source before allocating anything new, even across schedule batches.
    previous = ''
    if trusted:
        _, previous, context = trusted[0]
        if not completed_source(previous):
            return previous, False
        require(subprocess.run(['git', 'merge-base', '--is-ancestor', context['ci_source_commit'], base],
                               cwd=release.ROOT, capture_output=True).returncode == 0,
                'unstarted Nightly source predates the previous main source')
        require(batch[:8] >= versions_at(previous)['cli'].split('-nightly.')[1].split('.')[0],
                'unstarted Nightly batch predates the previous Nightly; refusing a downgrade')
        if context['ci_source_commit'] == base:
            return previous, True
    plan = plan_versions(batch, base, tags, run_id, previous)
    with checkout(base) as clean:
        for path, content in generated(plan).items():
            (clean / path).write_bytes(content)
        metadata(clean, plan)
        git('add', '--', *PATHS, root=clean)
        git('-c', 'user.name=github-actions[bot]', '-c', 'user.email=' + BOT,
            'commit', '-m', plan.message, root=clean)
        source = git('rev-parse', 'HEAD', root=clean)
        verify_tree(plan, source)
    return source, False


def run_daily(run_id):
    require(os.environ.get('GH_TOKEN'), 'GITHUB_TOKEN must be supplied as GH_TOKEN for tag/dispatch writes')
    require(os.environ.get('GITHUB_REPOSITORY') == REPO and os.environ.get('GITHUB_EVENT_NAME') == 'schedule'
            and os.environ.get('GITHUB_REF') == 'refs/heads/main' and str(run_id) == os.environ.get('GITHUB_RUN_ID'),
            'daily coordinator only accepts its original main schedule run')
    original = api(f'actions/runs/{run_id}')
    source = os.environ.get('GITHUB_SHA')
    require(original['event'] == 'schedule' and original['head_branch'] == 'main' and original['path'] == NIGHTLY
            and original['head_sha'] == source and original['repository']['full_name'] == REPO
            and original['head_repository']['full_name'] == REPO and str(original['id']) == str(run_id),
            'untrusted daily schedule source')
    batch = schedule_batch(original['created_at'])
    ci = release.require_ci(dict(ci_source_commit=source, ci_run_id=str(run_id), nightly_batch=batch))
    source, complete = ensure_source(batch, source, run_id)
    if complete:
        print(json.dumps(dict(batch=batch, source_commit=source, skipped='no new main commits; previous Nightly including npm is complete')))
        return
    context = release.validate_source(source, 'product')
    ci = release.require_ci(context)
    groups = RELEASES if context['ci_run_id'] else ('product',)
    deadline = time.monotonic() + 5 * 60 * 60
    with checkout(source) as clean:
        previous_root = release.ROOT
        release.ROOT = clean
        try:
            versions = versions_at(source)
            for app in groups:
                tag = release_dispatch_tag(app)
                require(release.parse_tag(tag)[2] == 'nightly', 'coordinator only publishes Nightly groups')
                release.plan(app, tag=tag, require_main=True)
            releases = publish_daily(source, deadline, groups)
        finally:
            release.ROOT = previous_root
    print(json.dumps(dict(batch=batch, source_commit=source, versions=versions,
                          schedule_ci=ci['html_url'], releases=releases), indent=2))


def self_test_source_layouts():
    """Historical and native source trees use their own exact version manifests."""
    from unittest.mock import patch

    old = '0.0.1-nightly.20261105.2'
    new = '0.0.1-nightly.20261106.1'
    for native in (False, True):
        with tempfile.TemporaryDirectory(dir=scratch(), prefix='source-layout-') as temporary:
            root = Path(temporary)
            rust = {'core': 'vibeface-core', 'cli': 'vibeface'}
            if native:
                rust['desktop'] = 'vibeface-desktop'
            initial = {}
            for app in release.APPS:
                directory = root / 'apps' / app
                directory.mkdir(parents=True)
                if app in rust:
                    path = f'apps/{app}/Cargo.toml'
                    text = f'[package]\nname = "{rust[app]}"\nversion = "{old}"\nedition = "2021"\n'
                    (directory / 'src').mkdir()
                    (directory / 'src/main.rs').write_text('fn main() {}\n')
                else:
                    path = f'apps/{app}/package.json'
                    value = {'version': old, 'private': True}
                    if app == 'mobile':
                        path = 'apps/mobile/app.json'
                        value = {'expo': {'version': '0.0.1', 'extra': {'releaseVersion': old},
                                          'android': {'versionCode': 1}, 'ios': {'buildNumber': '1'}}}
                    text = json.dumps(value, indent=2) + '\n'
                initial[path] = text.encode()
            initial['Cargo.lock'] = ('version = 3\n' + ''.join(
                f'\n[[package]]\nname = "{name}"\nversion = "{old}"\n' for name in sorted(rust.values()))).encode()
            for path, content in initial.items():
                (root / path).write_bytes(content)
            members = ', '.join(json.dumps(f'apps/{app}') for app in rust)
            (root / 'Cargo.toml').write_text(f'[workspace]\nmembers = [{members}]\nresolver = "2"\n')
            (root / 'business.txt').write_text('unchanged business source\n')
            git('init', '--initial-branch=main', root=root)
            git('add', '.', root=root)
            git('-c', 'user.name=github-actions[bot]', '-c', 'user.email=' + BOT,
                'commit', '-qm', 'immutable source layout', root=root)
            with patch('release.ROOT', root):
                base = git('rev-parse', 'HEAD')
                git('update-ref', 'refs/remotes/origin/main', base)
                plan = DailyPlan('2026110600', base, dict.fromkeys(release.APPS, new), 2, '2', '42')
                # Independent expected bytes, including unchanged manifest bodies and lock entries.
                expected = {path: content.replace(old.encode(), new.encode()) for path, content in initial.items()}
                expected['apps/mobile/app.json'] = expected['apps/mobile/app.json'].replace(
                    b'"versionCode": 1', b'"versionCode": 2').replace(b'"buildNumber": "1"', b'"buildNumber": "2"')
                assert generated(plan) == expected
                desktop_path = 'apps/desktop/' + ('Cargo.toml' if native else 'package.json')
                assert len(expected['Cargo.lock'].split(b'[[package]]')) - 1 == len(rust)
                if native:
                    assert set(expected) == set(PATHS) and len(rust) == 3
                else:
                    assert 'apps/desktop/Cargo.toml' not in expected and len(rust) == 2

                def commit_candidate(changes):
                    git('checkout', '--detach', '-q', base)
                    for path, content in (expected | changes).items():
                        (root / path).write_bytes(content)
                    git('add', '--all')
                    git('-c', 'user.name=github-actions[bot]', '-c', 'user.email=' + BOT,
                        'commit', '-qm', plan.message)
                    return git('rev-parse', 'HEAD')

                head = commit_candidate({})
                assert versions_at(head) == plan.versions
                assert recovered_plan(head) == plan
                verify_tree(plan, head)
                metadata(root, plan)
                assert release.validate_source(head, 'product') == dict(
                    ci_source_commit=base, ci_run_id='42', nightly_batch=plan.batch)
                tampered_manifest = (expected[desktop_path] + b'\n[dependencies]\nserde = "1"\n'
                                     if native else expected[desktop_path].replace(b'"private": true', b'"private": false'))
                for path, content in (
                    (desktop_path, tampered_manifest),
                    (desktop_path, expected[desktop_path].replace(new.encode(), b'0.0.1-nightly.20261106.2')),
                    ('Cargo.lock', expected['Cargo.lock'].replace(new.encode(), b'0.0.1-nightly.20261106.2', 1)),
                    ('Cargo.lock', expected['Cargo.lock'] + b'\n[[package]]\nname = "unexpected"\nversion = "1.0.0"\n'),
                    ('business.txt', b'modified non-version source\n'),
                ):
                    bad = commit_candidate({path: content})
                    for check in (lambda: verify_tree(plan, bad), lambda: release.validate_source(bad, 'product')):
                        try:
                            check()
                        except ValueError:
                            pass
                        else:
                            raise AssertionError(f'tampered source accepted: {path}')


def self_test():
    from unittest.mock import patch

    self_test_source_layouts()

    with patch.dict(os.environ, {'GH_TOKEN': 'fixture-actions-token'}):
        with patch('subprocess.run', return_value=subprocess.CompletedProcess([], 0, '{}', '')) as transport:
            api('git/refs', 'POST', dict(ref='refs/tags/v0.0.1', sha='a' * 40))
            assert transport.call_args.kwargs['env']['GH_TOKEN'] == 'fixture-actions-token'

    def rejected(call, message=None):
        try:
            call()
        except ValueError as error:
            assert message is None or message in str(error), str(error)
        else:
            raise AssertionError('unsafe operation was accepted')

    for package in ('vibeface', 'vibeface-core', 'vibeface-desktop'):
        lock_block = f'\nname = "{package}"\nversion = "0.0.1"\ndependencies = [\n "vibeface-protocol",\n]\n'
        updated = locked_package_version(lock_block, '0.0.1', '0.0.1-nightly.20261106.1')
        assert updated == lock_block.replace('version = "0.0.1"', 'version = "0.0.1-nightly.20261106.1"')
        for invalid in (lock_block.replace('version = "0.0.1"', '# version = "0.0.1"\nversion = "0.0.2"'),
                        lock_block + 'version = "0.0.1"\n',
                        lock_block + 'source = "registry+https://example.invalid/index"\n',
                        lock_block + 'checksum = "unchanged-dependency-digest"\n'):
            rejected(lambda invalid=invalid: locked_package_version(invalid, '0.0.1', '0.0.1-nightly.20261106.1'), 'Cargo.lock local package')

    for hour in range(24):
        assert schedule_batch(f'2026-11-06T{hour:02}:23:00Z') == f'20261106{hour // 4 * 4:02}'
        assert schedule_batch(f'2026-11-06T{hour:02}:59:59Z') == f'20261106{hour // 4 * 4:02}'
    assert schedule_batch('2026-11-07T00:23:00+08:00') == '2026110616'
    assert schedule_batch('2026-11-07T00:00:00Z') == '2026110700'
    for batch in ('20261106', '2026110601', '2026110624', '2026130600'):
        rejected(lambda: DailyPlan(batch, '', {}, 1, '3', '42'))

    with tempfile.TemporaryDirectory(dir=scratch(), prefix='self-test-') as temporary:
        root = Path(temporary) / 'repo'; root.mkdir()
        for app in release.APPS:
            directory = root / 'apps' / app; directory.mkdir(parents=True)
            if app in ('core', 'cli', 'desktop'):
                (directory / 'src').mkdir(); (directory / 'src/main.rs').write_text('fn main() {}\n')
                (directory / 'Cargo.toml').write_text('[package]\nname = "' + {'cli': 'vibeface', 'core': 'vibeface-core', 'desktop': 'vibeface-desktop'}[app]
                                                     + '"\nversion = "0.0.1-nightly.20261105.2"\nedition = "2021"\n')
            else:
                value = {'version': '0.0.1'} if app == 'www' else {'version': '0.0.1-nightly.20261105.2', 'private': True}
                if app == 'mobile':
                    value = {'expo': {'version': '0.0.1', 'ios': {'buildNumber': '3', 'supportsTablet': True},
                        'android': {'package': 'one.leaper.vibeface', 'versionCode': 250001},
                        'extra': {'releaseVersion': '0.0.1-nightly.20261105.2'}}}
                (directory / ('app.json' if app == 'mobile' else 'package.json')).write_text(json.dumps(value, indent=2) + '\n')
        (root / 'Cargo.toml').write_text('[workspace]\nmembers = ["apps/core", "apps/cli", "apps/desktop"]\nresolver = "2"\n')
        (root / 'Cargo.lock').write_text('version = 3\n\n[[package]]\nname = "vibeface"\nversion = "0.0.1-nightly.20261105.2"\n\n[[package]]\nname = "vibeface-core"\nversion = "0.0.1-nightly.20261105.2"\n\n[[package]]\nname = "vibeface-desktop"\nversion = "0.0.1-nightly.20261105.2"\n')
        git('init', '--initial-branch=main', root=root)
        legacy_blob = git('hash-object', '-w', '--stdin', root=root, data=(root / 'Cargo.lock').read_bytes())
        legacy_tree = git('mktree', root=root, data=f'100644 blob {legacy_blob}\tCargo.lock\n'.encode())
        legacy = git('-c', 'user.name=github-actions[bot]', '-c', 'user.email=' + BOT,
                     'commit-tree', legacy_tree, '-m', 'old release layout fixture', root=root)
        legacy_batch = git('-c', 'user.name=github-actions[bot]', '-c', 'user.email=' + BOT,
                           'commit-tree', legacy_tree, '-p', legacy, '-m', 'chore(release): nightly 2026110600', root=root)
        git('update-ref', 'refs/heads/main', legacy_batch, root=root)
        git('tag', 'v0.0.1-nightly.20261016.1', legacy, root=root)
        git('tag', 'v0.0.1-nightly.20261016.2', legacy_batch, root=root)
        (root / '.github/workflows').mkdir(parents=True)
        # Commit the historical contracts themselves; HEAD may already use the split entries.
        (root / CI).write_text("name: CI\non:\n  schedule:\n    - cron: '23 */4 * * *'\n"
                              "jobs:\n  ci-gate:\n    runs-on: ubuntu-24.04\n"
                              "    steps:\n      - run: 'true'\n")
        (root / RELEASE).write_text("name: Release\non:\n  workflow_dispatch:\n    inputs:\n"
                                   "      app:\n        type: choice\n        options: [product, www]\n"
                                   "      tag:\n        type: string\n"
                                   "      publish:\n        type: boolean\n        default: false\n"
                                   "jobs:\n  identity:\n    runs-on: ubuntu-24.04\n"
                                   "    steps:\n      - run: 'true'\n")
        git('add', '.', root=root)
        git('-c', 'user.name=github-actions[bot]', '-c', 'user.email=' + BOT, 'commit', '-qm', 'historical workflow fixture', root=root)
        old_base = git('rev-parse', 'HEAD', root=root)
        for path in (CI, RELEASE, NIGHTLY, '.github/workflows/release-www.yml'):
            (root / path).write_bytes((ROOT / path).read_bytes())
        git('add', '.', root=root)
        git('-c', 'user.name=github-actions[bot]', '-c', 'user.email=' + BOT, 'commit', '-qm', 'fixture', root=root)
        with patch('release.ROOT', root), patch(f'{__name__}.time.sleep'):
            assert RELEASES == ('product', 'www') and PRODUCT_APPS == ('app', 'core', 'cli', 'mobile', 'desktop')
            base = git('rev-parse', 'HEAD')
            git('update-ref', 'refs/remotes/origin/main', base)
            plan = plan_versions('2026110600', base, ['v0.0.1-nightly.20261106.7', 'core-v0.0.1-nightly.20261106.9'], '42')
            assert plan.versions['cli'] == '0.0.1-nightly.20261106.8'
            assert plan.versions['www'] == '0.0.1-nightly.20261106.1'
            assert plan.ios_build_number == '4' and plan.android_code == 250002
            assert next_ios_build_number('3.1.9') == '3.1.10'
            assert next_ios_build_number('3.01') == '3.2'
            for invalid in ('', '0', '10000', '9999', '3.99', '3.1.99', '3.100', '3.1.100', '3.1.2.3'):
                rejected(lambda invalid=invalid: next_ios_build_number(invalid))
            contents = generated(plan)
            mobile_generated = json.loads(contents['apps/mobile/app.json'])['expo']
            assert mobile_generated['ios']['buildNumber'] == '4'
            assert mobile_generated['ios']['supportsTablet'] is True
            assert re.search(rb'version = "([^"]+)"', contents['apps/desktop/Cargo.toml'])[1].decode() == plan.versions['cli']
            assert contents['Cargo.lock'].count(b'[[package]]') == 3
            assert 'apps/desktop/Cargo.toml' in PATHS and 'apps/desktop/package.json' not in PATHS
            for package in ('vibeface', 'vibeface-core', 'vibeface-desktop'):
                assert f'name = "{package}"\nversion = "{plan.versions["cli"]}"'.encode() in contents['Cargo.lock']

            def commit_files(changes, candidate=plan):
                git('checkout', '--detach', '-q', candidate.base)
                for path, content in generated(candidate).items():
                    (root / path).write_bytes(content)
                for path, content in changes.items():
                    (root / path).write_bytes(content)
                git('add', '--all')
                git('-c', 'user.name=github-actions[bot]', '-c', 'user.email=' + BOT, 'commit', '-qm', candidate.message)
                return git('rev-parse', 'HEAD')

            head = commit_files({})
            assert recovered_plan(head) == plan
            verify_tree(plan, head)
            metadata(root, plan)
            assert release.validate_source(head, 'product') == dict(ci_source_commit=base, ci_run_id='42', nightly_batch=plan.batch)
            assert release.validate_source(head, 'www')['ci_source_commit'] == base
            for path, content in (
                ('apps/app/package.json', contents['apps/app/package.json'].replace(b'"private": true', b'"private": false')),
                ('apps/desktop/Cargo.toml', contents['apps/desktop/Cargo.toml'] + b'\n[dependencies]\nserde = "1"\n'),
                ('Cargo.lock', contents['Cargo.lock'].replace(
                    f'name = "vibeface-desktop"\nversion = "{plan.versions["desktop"]}"'.encode(),
                    b'name = "vibeface-desktop"\nversion = "0.0.1-nightly.20261106.9"')),
                ('Cargo.lock', contents['Cargo.lock'] + b'\n[[package]]\nname = "unexpected"\nversion = "1.0.0"\n'),
                ('apps/core/Cargo.toml', contents['apps/core/Cargo.toml'] + b'\n[dependencies]\nserde = "1"\n'),
                ('apps/cli/Cargo.toml', contents['apps/cli/Cargo.toml'].replace(b'20261106.8', b'20261106.9')),
                ('apps/mobile/app.json', contents['apps/mobile/app.json'].replace(b'250002', b'1')),
                ('apps/mobile/app.json', contents['apps/mobile/app.json'].replace(b'"buildNumber": "4"', b'"buildNumber": "5"')),
                ('apps/mobile/app.json', contents['apps/mobile/app.json'].replace(b'"supportsTablet": true', b'"supportsTablet": false')),
                ('extra.txt', b'not a version file\n'),
            ):
                bad = commit_files({path: content})
                rejected(lambda: verify_tree(plan, bad), 'mechanical')
                rejected(lambda: release.validate_source(bad, 'product'))
            stale = commit_files({'apps/mobile/app.json': contents['apps/mobile/app.json'].replace(b'20261106.8', b'20261106.9')})
            rejected(lambda: release.validate_source(stale, 'product'), 'canonical version')
            git('checkout', '--detach', '-q', base)
            (root / 'business.txt').write_text('main advanced independently\n')
            git('add', 'business.txt')
            git('-c', 'user.name=github-actions[bot]', '-c', 'user.email=' + BOT, 'commit', '-qm', 'independent main change')
            advanced = git('rev-parse', 'HEAD')
            git('update-ref', 'refs/remotes/origin/main', advanced)
            assert recovered_plan(head).base == base and release.validate_source(head, 'product')['ci_source_commit'] == base
            assert release.validate_source(advanced, 'product')['ci_source_commit'] == advanced
            merge = git('-c', 'user.name=github-actions[bot]', '-c', 'user.email=' + BOT,
                        'commit-tree', git('rev-parse', head + '^{tree}'), '-p', base, '-p', advanced, '-m', plan.message)
            rejected(lambda: release.validate_source(merge, 'product'), 'ordinary commit')
            foreign = git('-c', 'user.name=github-actions[bot]', '-c', 'user.email=' + BOT,
                          'commit-tree', git('rev-parse', head + '^{tree}'), '-m', plan.message)
            rejected(lambda: release.validate_source(foreign, 'product'), 'ordinary commit')
            orphan_base = git('-c', 'user.name=github-actions[bot]', '-c', 'user.email=' + BOT,
                              'commit-tree', git('rev-parse', base + '^{tree}'), '-m', 'foreign root')
            orphan = DailyPlan(plan.batch, orphan_base, plan.versions, plan.android_code, plan.ios_build_number, '42')
            orphan_head = commit_files({}, orphan)
            rejected(lambda: release.validate_source(orphan_head, 'product'), 'parent must be on main')
            stable = commit_files({'apps/www/package.json': contents['apps/www/package.json'].replace(b'0.0.1-nightly.20261106.1', b'0.0.1')})
            rejected(lambda: release.validate_source(stable, 'www'), 'only Nightly')
            for value in (b'0.0.1', b'0.0.1-rc.1'):
                ordinary_channel = commit_files({path: content.replace(plan.versions['cli'].encode(), value)
                                                 for path, content in contents.items()})
                rejected(lambda: release.validate_source(ordinary_channel, 'product'), 'only Nightly')
            git('tag', 'v' + plan.versions['cli'], head)
            # A new business parent still starts with main's old build values; the prior tag supplies the upper bounds.
            next_plan = plan_versions('2026110604', advanced, ['v' + plan.versions['cli']], '43', head)
            assert next_plan.android_code == 250003 and next_plan.ios_build_number == '5'
            next_head = commit_files({}, next_plan)
            assert release.validate_source(next_head, 'product')['ci_source_commit'] == advanced
            assert recovered_plan(next_head).previous == head
            legacy_tag = 'v0.0.1-nightly.20261016.1'
            current_tag = 'v' + plan.versions['cli']
            git('merge-base', '--is-ancestor', legacy, 'origin/main')
            for tags in ([legacy_tag, current_tag], [current_tag, legacy_tag]):
                assert [source for _, source, _ in trusted_nightlies(tags, plan.batch)] == [head]
            rejected(lambda: trusted_nightlies(['v0.0.1-nightly.20261016.2'], plan.batch), 'invalid immutable source')
            with patch('release.validate_source', side_effect=subprocess.CalledProcessError(1, ['cargo', 'metadata'])):
                try:
                    trusted_nightlies([current_tag], plan.batch)
                except subprocess.CalledProcessError as error:
                    assert error.cmd == ['cargo', 'metadata']
                else:
                    raise AssertionError('candidate filtering swallowed a metadata environment failure')
            rollback_plan = plan_versions('2026110608', base, ['v' + next_plan.versions['cli']], '44', next_head)
            rollback = commit_files({}, rollback_plan)
            for _ in range(2):
                rejected(lambda: release.validate_source(rollback, 'product'), 'previous Nightly main source must be an ancestor')
            nested_plan = plan_versions('2026110612', advanced, ['v' + rollback_plan.versions['cli']], '45', rollback)
            nested = commit_files({}, nested_plan)
            rejected(lambda: release.validate_source(nested, 'product'), 'previous Nightly main source must be an ancestor')
            future_main_plan = plan_versions('2026110608', base, [current_tag], '44', advanced)
            future_main = commit_files({}, future_main_plan)
            rejected(lambda: release.validate_source(future_main, 'product'), 'previous Nightly main source must be an ancestor')
            # Numeric CFBundleVersion ordering must select 3.1.10 rather than 3.1.9.
            with patch('nightly_source.blob', side_effect=lambda commit, path: json.dumps({'expo': {
                    'android': {'versionCode': 9 if commit == base else 10},
                    'ios': {'buildNumber': '3.1.9' if commit == base else '3.1.10'}}}).encode()):
                assert next_build_numbers(base, head) == (11, '3.1.11')
            with patch('nightly_source.blob', side_effect=lambda commit, path: json.dumps({'expo': {
                    'android': {'versionCode': 10}, 'ios': {'buildNumber': '4' if commit == base else '3.99'}}}).encode()):
                assert next_build_numbers(base, head) == (11, '5')
            rejected(lambda: plan_versions('2026110400', base, [], '42'), 'current cli version')

            # Recovery never allocates a new version or switches to the newly advanced main.
            remote_tags = f'{head}\trefs/tags/v{plan.versions["cli"]}'
            real_git = git
            def fixture_git(*args, **kwargs):
                if args[0] == 'fetch':
                    return ''
                if args[0] == 'ls-remote':
                    return remote_tags
                return real_git(*args, **kwargs)
            with patch(f'{__name__}.git', side_effect=fixture_git), patch(f'{__name__}.completed_source', return_value=False), \
                    patch(f'{__name__}.plan_versions', side_effect=AssertionError('original source reallocated')):
                assert ensure_source(plan.batch, base, 42) == (head, False)
                assert ensure_source('2026110604', base, 43) == (head, False)
                rejected(lambda: ensure_source(plan.batch, advanced, 42), 'original schedule source')
            with patch(f'{__name__}.git', side_effect=fixture_git), patch(f'{__name__}.completed_source', return_value=True), \
                    patch(f'{__name__}.plan_versions', side_effect=AssertionError('unchanged main reallocated')):
                assert ensure_source('2026110604', base, 43) == (head, True)
            git('tag', 'www-v' + plan.versions['www'], head)
            saved_tags = remote_tags
            remote_tags = f'{head}\trefs/tags/www-v{plan.versions["www"]}'
            with patch(f'{__name__}.git', side_effect=fixture_git), patch(f'{__name__}.completed_source', return_value=False), \
                    patch(f'{__name__}.plan_versions', side_effect=AssertionError('WWW-only source reallocated')):
                assert ensure_source('2026110604', advanced, 43) == (head, False)
            remote_tags = saved_tags
            git('tag', 'v' + next_plan.versions['cli'], next_head)
            remote_tags += f'\n{next_head}\trefs/tags/v{next_plan.versions["cli"]}'
            with patch(f'{__name__}.git', side_effect=fixture_git), patch(f'{__name__}.completed_source', return_value=True), \
                    patch(f'{__name__}.plan_versions', side_effect=AssertionError('stale batch allocated a new version')):
                rejected(lambda: ensure_source('2026110504', advanced, 44), 'predates the previous Nightly')
                rejected(lambda: ensure_source('2026110608', base, 44), 'predates the previous main source')
            with patch(f'{__name__}.git', side_effect=fixture_git), patch(f'{__name__}.completed_source', return_value=False), \
                    patch(f'{__name__}.plan_versions', side_effect=AssertionError('old batch was reallocated')):
                assert ensure_source(plan.batch, base, 42) == (head, False)
            remote_tags = saved_tags
            # The same shared source rule runs at the real plan and publisher boundaries.
            git('checkout', '--detach', '-q', head)
            assert release.plan('product', tag='v' + plan.versions['cli'], require_main=True, publishing=True)['ci_source_commit'] == base
            release.require_product_publishing()
            bad = commit_files({'apps/app/package.json': contents['apps/app/package.json'].replace(b'"private": true', b'"private": false')})
            git('update-ref', 'refs/tags/v' + plan.versions['cli'], bad)
            remote_tags = f'{bad}\trefs/tags/v{plan.versions["cli"]}'
            with patch(f'{__name__}.git', side_effect=fixture_git), \
                    patch(f'{__name__}.plan_versions', side_effect=AssertionError('conflicting tag allocated a new version')):
                rejected(lambda: ensure_source(plan.batch, base, 42), 'invalid immutable source')
            remote_tags = saved_tags
            rejected(lambda: release.plan('product', require_main=True), 'mechanical')
            rejected(release.require_product_publishing, 'mechanical')
            with patch('release.verify_release', return_value=release.identity('product')), patch('release.release_view', side_effect=AssertionError('untrusted publication wrote remotely')):
                rejected(lambda: release.publish('product', root), 'mechanical')
            git('update-ref', 'refs/tags/v' + plan.versions['cli'], head)
            remote_tags = ''
            with patch(f'{__name__}.git', side_effect=fixture_git):
                pinned_source, complete = ensure_source('2026110600', base, 42)
            assert not complete and recovered_plan(pinned_source).base == base
            assert git('ls-tree', pinned_source, '--', 'business.txt') == ''
            remote_tags = saved_tags
            good = dict(id=1, run_attempt=1, head_sha=head, head_branch='main', event='push', path=CI,
                        repository=dict(full_name=REPO), head_repository=dict(full_name=REPO),
                        status='completed', conclusion='success', html_url='https://example.invalid/main-ci')
            schedule = dict(good, id=42, event='schedule', head_sha=base, created_at='2026-11-06T00:23:00Z',
                            status='in_progress', conclusion=None, path=NIGHTLY)
            ci_context = dict(ci_source_commit=base, ci_run_id='42', nightly_batch=plan.batch)
            jobs = [dict(name='Nightly validation gate', status='completed', conclusion='success')]
            original_run = release.run
            def ci_api(*args, **kwargs):
                if args[0] == 'git':
                    return original_run(*args, **kwargs)
                return json.dumps([dict(jobs=jobs)] if args[-1].endswith('per_page=100') else schedule)
            with patch('release.run', side_effect=ci_api):
                assert release.require_ci(ci_context)['id'] == 42
                for change in ({'id': 43}, {'head_sha': advanced}, {'event': 'workflow_dispatch'}, {'head_branch': 'feature'},
                               {'path': RELEASE}, {'repository': dict(full_name='other/repo')},
                               {'head_repository': dict(full_name='other/repo')}, {'created_at': '2026-11-06T04:23:00Z'}):
                    with patch.dict(schedule, change):
                        rejected(lambda: release.require_ci(ci_context))
                for job_list in ([], [dict(jobs[0], conclusion='failure')], [dict(jobs[0], conclusion='skipped')],
                                 [dict(jobs[0], status='in_progress')], [dict(name=jobs[0]['name'])], jobs * 2,
                                 [dict(jobs[0], name='checks / Nightly validation gate')]):
                    with patch('release.run', side_effect=lambda *args, **kwargs: original_run(*args, **kwargs) if args[0] == 'git'
                               else json.dumps([dict(jobs=job_list)] if args[-1].endswith('per_page=100') else schedule)):
                        rejected(lambda: release.require_ci(ci_context), 'Nightly validation gate')
                for pinned, path, gate in ((old_base, CI, 'ci-gate'), (base, NIGHTLY, 'Nightly validation gate')):
                    pinned_context = dict(ci_context, ci_source_commit=pinned)
                    pinned_run = dict(schedule, head_sha=pinned, path=path)
                    for crossed_path in (CI, NIGHTLY):
                        for crossed_gate in ('ci-gate', 'Nightly validation gate'):
                            def pinned_api(*args, **kwargs):
                                if args[0] == 'git':
                                    return original_run(*args, **kwargs)
                                return json.dumps([dict(jobs=[dict(jobs[0], name=crossed_gate)])]
                                                  if args[-1].endswith('per_page=100') else dict(pinned_run, path=crossed_path))
                            with patch('release.run', side_effect=pinned_api):
                                if crossed_path == path and crossed_gate == gate:
                                    assert release.require_ci(pinned_context)['head_sha'] == pinned
                                else:
                                    rejected(lambda: release.require_ci(pinned_context))
            assert release.release_workflow(old_base, 'product') == (RELEASE, dict(app='product'))
            assert release.release_workflow(old_base, 'www') == (RELEASE, dict(app='www'))
            assert release.release_workflow(base, 'product') == (RELEASE, {})
            assert release.release_workflow(base, 'www') == ('.github/workflows/release-www.yml', {})
            for pinned in (old_base, base):
                for app in RELEASES:
                    workflow, extra = release.release_workflow(pinned, app)
                    dispatch_tag = ('www-v' if app == 'www' else 'v') + '0.0.1-nightly.20261106.1'
                    dispatched = []; discovered = []
                    expected_run = dict(good, head_sha=pinned, path=workflow, head_branch=dispatch_tag,
                                        event='workflow_dispatch')
                    def dispatch_api(path, method='GET', data=None):
                        assert path == 'actions/workflows/' + workflow.split('/')[-1] + '/dispatches'
                        assert method == 'POST' and data == dict(ref=dispatch_tag, inputs=dict(extra, tag=dispatch_tag, publish='true'))
                        dispatched.append(data)
                        discovered.append(expected_run)
                        raise ValueError('dispatch accepted but response lost')
                    def dispatch_pages(path, key=None):
                        assert path == ('actions/workflows/' + workflow.split('/')[-1] + '/runs?head_sha='
                                        + pinned + '&branch=' + quote(dispatch_tag, safe=''))
                        assert key == 'workflow_runs'
                        return discovered
                    with patch(f'{__name__}.api', side_effect=dispatch_api), patch(f'{__name__}.pages', side_effect=dispatch_pages), \
                            patch(f'{__name__}.ensure_tag'), patch(f'{__name__}.release_state', return_value=None), \
                            patch(f'{__name__}.release_dispatch_tag', return_value=dispatch_tag):
                        assert start_release(app, pinned) == [expected_run]
                        assert start_release(app, pinned) == [expected_run] and len(dispatched) == 1
                        # Even a response from the selected endpoint must match its exact source-era path.
                        for wrong in (dict(expected_run, path=CI), dict(expected_run, head_sha=advanced),
                                      dict(expected_run, event='schedule'), dict(expected_run, head_branch='main')):
                            discovered[:] = [wrong]
                            assert matching_runs(workflow, pinned, dispatch_tag, {'push', 'workflow_dispatch'}) == []
            rejected(lambda: release.nightly_ci_identity(legacy), 'missing Core CI')
            rejected(lambda: release.nightly_ci_identity('HEAD'), 'invalid workflow source')
            rejected(lambda: release.nightly_ci_identity(base[:12]), 'invalid workflow source')
            workflow_blob = git('rev-parse', base + ':' + CI)
            malformed_workflows = git('mktree', data=(f'100644 blob {workflow_blob}\tci.yml\n'
                f'120000 blob {workflow_blob}\tnightly.yml\n100644 blob {workflow_blob}\trelease.yml\n').encode())
            malformed_github = git('mktree', data=f'040000 tree {malformed_workflows}\tworkflows\n'.encode())
            malformed_tree = git('mktree', data=f'040000 tree {malformed_github}\t.github\n'.encode())
            malformed_source = git('-c', 'user.name=github-actions[bot]', '-c', 'user.email=' + BOT,
                                   'commit-tree', malformed_tree, '-m', 'malformed workflow tree fixture')
            rejected(lambda: release.nightly_ci_identity(malformed_source), 'malformed workflow source tree')
            rejected(lambda: release.release_workflow(malformed_source, 'www'), 'malformed workflow source tree')
            with patch('release.run', return_value=json.dumps([dict(workflow_runs=[dict(good, head_sha=base)])])):
                assert release.require_ci(dict(ci_source_commit=base, ci_run_id='', nightly_batch=''))['id'] == 1
                for change in ({'event': 'schedule'}, {'head_sha': advanced}, {'path': RELEASE}, {'path': NIGHTLY},
                               {'status': 'in_progress'}, {'conclusion': 'failure'}):
                    with patch('release.run', return_value=json.dumps([dict(workflow_runs=[dict(good, **({'head_sha': base} | change))])])):
                        rejected(lambda: release.require_ci(dict(ci_source_commit=base, ci_run_id='', nightly_batch='')))
            with patch(f'{__name__}.tag_sha', return_value=head), patch(f'{__name__}.release_state', return_value=dict(draft=False)), \
                    patch(f'{__name__}.matching_runs', return_value=[good]), patch('release.verify_public') as verify_public:
                for state in ('success', 'failure', 'skipped'):
                    with patch(f'{__name__}.pages', return_value=[dict(name='npm / npm', status='completed', conclusion=state)]):
                        assert completed_source(head) == (state == 'success')
                assert verify_public.call_count == 2
                with patch(f'{__name__}.pages', return_value=[]):
                    assert not completed_source(head)
                with patch(f'{__name__}.tag_sha', return_value=None):
                    assert not completed_source(head)
            with patch(f'{__name__}.tag_sha', return_value=base), patch(f'{__name__}.release_state', return_value=dict(draft=False)), \
                    patch(f'{__name__}.matching_runs', return_value=[good]), patch(f'{__name__}.npm_complete', return_value=True), \
                    patch('release.verify_public') as verified:
                assert completed_source(base)
                assert verified.call_count == 1 and verified.call_args.args[0] == 'product'
            merged = head
            git('checkout', '--detach', '-q', advanced)

            assert release_dispatch_tag('product') == 'v0.0.1-nightly.20261105.2'
            assert release_dispatch_tag('www') == 'www-v0.0.1'
            for group in ('mobile', 'app', 'core', 'cli'):
                rejected(lambda: release_dispatch_tag(group), 'product and www release groups')
            git('checkout', '--detach', '-q', stale)
            rejected(lambda: release_dispatch_tag('product'), 'mobile version must equal')
            git('checkout', '--detach', '-q', advanced)

            writes = []; remote = {'tag': None, 'runs': [], 'public': False}
            tag = 'v' + plan.versions['cli']

            def release_api(path, method='GET', data=None, **kwargs):
                if method != 'GET':
                    writes.append((path, method, data))
                    if path.endswith('/dispatches'):
                        # A real daily release must request publication, not a build-only dry run.
                        assert data == dict(ref=tag, inputs=dict(tag=tag, publish='true'))
                        remote['runs'] = [dict(good, head_branch=tag, event='workflow_dispatch', path=RELEASE)]
                    else:
                        raise AssertionError('unexpected write')
                    raise ValueError('write accepted but response lost')
                if path.startswith('git/ref/tags/'):
                    return dict(object=dict(type='commit', sha=remote['tag'])) if remote['tag'] else None
                assert path.startswith('releases/tags/')
                return dict(draft=False) if remote['public'] else None

            def tag_push(*args, **kwargs):
                assert args == ('push', 'origin', merged + ':refs/tags/' + tag)
                writes.append(('tag-push', 'git', args)); remote['tag'] = merged
                raise subprocess.CalledProcessError(1, ['git', 'push'])

            with patch(f'{__name__}.api', side_effect=release_api), patch(f'{__name__}.git', side_effect=tag_push), \
                    patch(f'{__name__}.matching_runs', side_effect=lambda *args: remote['runs']), \
                    patch(f'{__name__}.release_dispatch_tag', return_value=tag):
                ensure_tag(tag, merged); ensure_tag(tag, merged)
                assert len(writes) == 1
                assert start_release('product', merged) and len(writes) == 2
                assert start_release('product', merged) and len(writes) == 2
                remote['public'] = True; remote['runs'] = []
                assert start_release('product', merged) == [] and len(writes) == 2
                remote['tag'] = 'b' * 40
                rejected(lambda: start_release('product', merged), 'different SHA')
                assert len(writes) == 2
            attempts = {'value': dict(good, conclusion='failure')}; writes = []

            def recovery_api(path, method='GET', **kwargs):
                if method == 'POST':
                    writes.append(path)
                    attempts['value'] = dict(good, run_attempt=2, status='queued', conclusion=None)
                    raise ValueError('rerun response lost')
                return attempts['value']

            produced = dict(workflow_run=dict(id=1, head_sha=merged), expired=False, size_in_bytes=100)
            product_prefixes = [f'static-release-{merged}-',
                                *(f'native-release-{target}-{merged}-' for target in release.TARGETS),
                                *(f'desktop-release-{target}-{merged}-' for target in release.TARGETS),
                                f'android-release-{merged}-', f'ios-device-archive-{merged}-',
                                f'product-release-{merged}-', f'npm-package-{merged}-']
            good_artifacts = [dict(produced, id=51 + index, name=prefix + '1')
                              for index, prefix in enumerate(product_prefixes)]
            remote_state = {
                'artifacts': good_artifacts,
                'jobs': [dict(name='collect', status='completed', conclusion='success'),
                         dict(name='publish-product', status='completed', conclusion='success'),
                         dict(name='npm / npm', status='completed', conclusion='failure')],
            }

            def recovery_pages(path, key):
                return list(remote_state['artifacts' if key == 'artifacts' else 'jobs'])

            failing = dict(good, conclusion='failure')
            with patch(f'{__name__}.api', side_effect=recovery_api), patch(f'{__name__}.pages', side_effect=recovery_pages):
                # A public product Release whose npm job failed is recovered from the original immutable artifacts.
                recovered_ids = original_artifacts('product', failing, merged)
                assert set(recovered_ids) == set(product_prefixes)
                assert recovered_ids[f'product-release-{merged}-'] == 51 + product_prefixes.index(f'product-release-{merged}-')
                assert recover_publisher('product', failing, merged)['run_attempt'] == 2
                assert writes == ['actions/runs/1/rerun-failed-jobs']
                for artifacts in ([], good_artifacts[:-1],
                                  [dict(item, name=(item['name'][:-1] + '2') if item['name'].startswith(f'npm-package-{merged}-') else item['name']) for item in good_artifacts],
                                  [dict(item, expired=True) if item['name'].startswith(f'npm-package-{merged}-') else item for item in good_artifacts],
                                  [dict(item, workflow_run=dict(id=9, head_sha=merged)) if item['name'].startswith(f'npm-package-{merged}-') else item for item in good_artifacts],
                                  [dict(item, workflow_run=dict(id=1, head_sha='b' * 40)) if item['name'].startswith(f'npm-package-{merged}-') else item for item in good_artifacts]):
                    with patch.dict(remote_state, {'artifacts': artifacts}):
                        rejected(lambda: recover_publisher('product', failing, merged), 'refusing a rebuild')
                for jobs in ([dict(name='collect', status='completed', conclusion='success'),
                              dict(name='compose-macos', status='completed', conclusion='failure')],
                             [dict(name='collect', status='completed', conclusion='success'),
                              dict(name='npm / npm', status='in_progress', conclusion=None)]):
                    with patch.dict(remote_state, {'jobs': jobs}):
                        rejected(lambda: recover_publisher('product', failing, merged), 'named original-artifact publisher')
                product_publishers = ('release-start', *PART_PUBLISHERS, 'publish-product', 'npm')
                for label in product_publishers:
                    with patch.dict(remote_state, {'jobs': [dict(name='collect', status='completed', conclusion='success'),
                                                           dict(name='publish-product', status='completed', conclusion='success'),
                                                           dict(name=f'{label} / {label}', status='completed', conclusion='failure')]}):
                        assert recover_publisher('product', failing, merged)['run_attempt'] == 2
                for jobs in ([dict(name='collect', status='completed', conclusion='failure'),
                              dict(name='publish-product', status='completed', conclusion='failure')],
                             [dict(name='publish-product', status='completed', conclusion='failure')]):
                    with patch.dict(remote_state, {'jobs': jobs}):
                        rejected(lambda: recover_publisher('product', failing, merged), 'audit did not succeed')
                rejected(lambda: recover_publisher('mobile', failing, merged), 'product and www')
                assert len(writes) == 1 + len(product_publishers)

            with patch(f'{__name__}.npm_complete', return_value=True), patch(f'{__name__}.tag_sha', return_value=merged), patch(f'{__name__}.release_state', return_value=dict(draft=False)), \
                    patch('release.verify_public') as verified, patch(f'{__name__}.recover_publisher') as no_retry, \
                    patch(f'{__name__}.release_dispatch_tag', return_value='v' + plan.versions['cli']):
                with patch(f'{__name__}.matching_runs', return_value=[]):
                    rejected(lambda: finish_release('product', merged, time.monotonic() + 1), 'no original workflow run')
                verified.assert_not_called()
                no_retry.assert_not_called()
                # A failed npm job keeps the product outcome red even though the Release is already public.
                with patch(f'{__name__}.matching_runs', return_value=[failing]), patch(f'{__name__}.time.sleep'):
                    rejected(lambda: finish_release('product', merged, time.monotonic() + 1), 'not complete')
                assert no_retry.call_count == 1
                verified.assert_not_called()
                with patch(f'{__name__}.matching_runs', side_effect=[[dict(good, status='in_progress', conclusion=None)], [dict(good, run_attempt=2)]]), \
                        patch(f'{__name__}.time.sleep'):
                    assert finish_release('product', merged, time.monotonic() + 1)[0]['conclusion'] == 'success'
                verified.assert_called_once()

            with patch(f'{__name__}.npm_complete', return_value=True), patch(f'{__name__}.tag_sha', return_value=merged), patch(f'{__name__}.release_state', return_value=dict(draft=False)), \
                    patch('release.verify_public', side_effect=ValueError('canonical completion manifest is missing')), \
                    patch(f'{__name__}.release_dispatch_tag', return_value='v' + plan.versions['cli']), \
                    patch(f'{__name__}.matching_runs', return_value=[good]):
                rejected(lambda: finish_release('product', merged, time.monotonic() + 1), 'completion manifest is missing')

            with patch(f'{__name__}.npm_complete', return_value=True), patch(f'{__name__}.tag_sha', return_value=merged), patch(f'{__name__}.release_state', return_value=None), \
                    patch('release.verify_public') as verified, patch(f'{__name__}.recover_publisher') as no_retry, \
                    patch(f'{__name__}.release_dispatch_tag', return_value='v' + plan.versions['cli']), \
                    patch(f'{__name__}.matching_runs', return_value=[failing]), patch(f'{__name__}.time.sleep'):
                rejected(lambda: finish_release('product', merged, time.monotonic() + 1), 'not complete')
                verified.assert_not_called()
                assert no_retry.call_count == 1

            for blocked in ('product', 'www', None):
                operations = []
                def finish(app, *args):
                    operations.append(('verified', app))
                    require(app != blocked, 'release group is not public or verified')
                    return []
                with patch(f'{__name__}.start_release', side_effect=lambda app, *args: operations.append(('started', app))), \
                        patch(f'{__name__}.finish_release', side_effect=finish), patch(f'{__name__}.matching_runs', return_value=[]):
                    if blocked:
                        rejected(lambda: publish_daily(merged, 1), 'not public')
                        # www is independent: the other group is still started and reconciled.
                        assert [app for operation, app in operations if operation == 'started'] == list(RELEASES)
                        assert {app for operation, app in operations if operation == 'verified'} == set(RELEASES)
                    else:
                        assert set(publish_daily(merged, 1)) == {'product', 'www'}
                        assert operations[:2] == [('started', 'product'), ('started', 'www')]
            observed = []
            def drain(*args):
                tag = args[2]
                observed.append(tag)
                n = sum(1 for t in observed if t == tag)
                return [dict(good, status='in_progress', conclusion=None)] if n == 1 else [good]
            with patch(f'{__name__}.start_release'), patch(f'{__name__}.finish_release', side_effect=ValueError('release group failed')), \
                    patch(f'{__name__}.matching_runs', side_effect=drain), patch(f'{__name__}.time.monotonic', return_value=0):
                rejected(lambda: publish_daily(merged, 1), 'release group failed')
                assert len(observed) == 4
                assert observed[0] == observed[1] and observed[2] == observed[3]
                groups = {observed[0], observed[2]}
                assert len(groups) == 2
                assert any(t.startswith('v') and t[1] in '0123456789' for t in groups)
                assert any(t.startswith('www-v') for t in groups)
                assert not any(t.startswith('mobile-v') or t.startswith('app-v') or t.startswith('core-v') for t in groups)

            environment = dict(GITHUB_REPOSITORY=REPO, GITHUB_EVENT_NAME='schedule', GITHUB_REF='refs/heads/main',
                               GITHUB_RUN_ID='42', GITHUB_SHA=base, GH_TOKEN='fixture-only')
            observed_batches = []
            def observe_batch(batch, pinned, run_id):
                observed_batches.append((batch, pinned, run_id))
                raise ValueError('batch recovered from original run')
            for attempt in (1, 2):
                schedule['run_attempt'] = attempt
                with patch.dict(os.environ, environment), patch(f'{__name__}.api', return_value=schedule), \
                        patch('release.require_ci', return_value=schedule), patch(f'{__name__}.ensure_source', side_effect=observe_batch):
                    rejected(lambda: run_daily(42), 'batch recovered')
            assert observed_batches == [('2026110600', base, 42)] * 2
            for changes in ({'GH_TOKEN': ''}, {'GITHUB_EVENT_NAME': 'workflow_dispatch'}):
                with patch.dict(os.environ, dict(environment, **changes)), patch(f'{__name__}.api') as no_write:
                    rejected(lambda: run_daily(42))
                    no_write.assert_not_called()

    workflow = (ROOT / NIGHTLY).read_text()
    assert "cron: '23 */4 * * *'" in workflow and 'group: daily-nightly\n      cancel-in-progress: false' in workflow
    assert "if: ${{ !cancelled() && github.event_name == 'schedule' && github.ref == 'refs/heads/main' && needs.ci-gate.result == 'success' }}" in workflow
    npm = (ROOT / '.github/workflows/npm-publish.yml').read_text()
    assert 'id-token: write' in npm and 'environment: npm-release' in npm and 'default: false' in npm
    assert 'NPM_TOKEN' not in npm and 'secrets.' not in npm
    nightly_job = workflow.split('  daily-nightly:', 1)[1]
    assert 'NIGHTLY_RELEASE_TOKEN' not in nightly_job and 'pull-requests:' not in nightly_job
    assert 'name: Release / Nightly\n' in workflow and 'run-name:' in workflow
    checks_job = workflow.split('  checks:', 1)[1].split('  ci-gate:', 1)[0]
    assert 'secrets: inherit' in checks_job, 'Nightly Core checks require the existing platform signing identities'
    assert 'uses: ./.github/workflows/ci.yml' in checks_job and 'package_policy:' not in checks_job
    assert workflow.count('uses: ./.github/workflows/ci.yml') == 1
    gate_job = workflow.split('  ci-gate:', 1)[1].split('  daily-nightly:', 1)[0]
    assert 'name: Nightly validation gate\n' in gate_job and 'needs: [checks]' in gate_job
    assert 'if: always()' in gate_job and 'checkout' not in gate_job
    assert "set(checks) != {'checks'}" in gate_job and ".get('result') != 'success'" in gate_job
    assert 'needs: [ci-gate]' in nightly_job and 'ref: ${{ github.sha }}' in nightly_job
    import textwrap
    gate_code = textwrap.dedent(gate_job.split("python3 - <<'PYTHON'\n", 1)[1].split('          PYTHON', 1)[0])
    for result in ({'checks': {'result': 'success'}}, {'checks': {'result': 'failure'}},
                   {'checks': {'result': 'cancelled'}}, {'checks': {'result': 'skipped'}},
                   {'checks': {}}, {}, {'unexpected': {'result': 'success'}}):
        tested = subprocess.run([sys.executable, '-c', gate_code], env=dict(os.environ, NIGHTLY_NEEDS=json.dumps(result)),
                                capture_output=True, text=True)
        assert (tested.returncode == 0) == (result == {'checks': {'result': 'success'}})
    print('four-hour Nightly real-Git, batch identity, reconciliation and original-artifact self-tests passed')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--self-test', action='store_true')
    parser.add_argument('--run-id', type=int, default=os.environ.get('GITHUB_RUN_ID'))
    args = parser.parse_args()
    if args.self_test:
        self_test()
    else:
        require(args.run_id, '--run-id or GITHUB_RUN_ID is required')
        run_daily(args.run_id)


if __name__ == '__main__':
    main()
