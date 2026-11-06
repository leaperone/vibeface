#!/usr/bin/env python3
"""Land one mechanical daily version PR, then reconcile five immutable Nightlies."""
import argparse
from contextlib import contextmanager
from dataclasses import dataclass
import datetime
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import time
from urllib.parse import quote

import release

ROOT = Path(__file__).resolve().parent.parent
REPO = 'leaperone/vibeface'
CI = '.github/workflows/ci.yml'
RELEASE = '.github/workflows/release.yml'
PATHS = ('apps/www/package.json', 'apps/app/package.json', 'apps/core/Cargo.toml',
         'apps/cli/Cargo.toml', 'Cargo.lock', 'apps/cli/release-lock.json', 'apps/mobile/app.json')
BOT = '41898282+github-actions[bot]@users.noreply.github.com'
require = release.require


@dataclass(frozen=True)
class DailyPlan:
    day: str
    base: str
    versions: dict
    android_code: int

    @property
    def branch(self):
        return 'bot/daily-nightly/' + self.day


def git(*args, root=None, data=None, env=None):
    return subprocess.check_output(['git', *args], cwd=root or ROOT, input=data, env=env).decode().strip()


def blob(commit, path):
    return subprocess.check_output(['git', 'show', f'{commit}:{path}'], cwd=ROOT)


def api(path, method='GET', data=None, pat=False, missing=False):
    environment = dict(os.environ)
    if pat:
        require(environment.get('NIGHTLY_RELEASE_TOKEN'), 'NIGHTLY_RELEASE_TOKEN is required; daily Nightly is not configured')
        environment['GH_TOKEN'] = environment['NIGHTLY_RELEASE_TOKEN']
    args = ['gh', 'api', '--method', method, 'repos/' + REPO + '/' + path]
    if path == '/user':
        args[-1] = 'user'
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


def json_value(text, path, value):
    data = json.loads(text)
    require(json.dumps(data, indent=2, ensure_ascii=False) + '\n' == text, 'version JSON formatting changed; refuse unrelated formatting edits')
    parent = data
    for key in path[:-1]:
        parent = parent[key]
    require(path[-1] in parent, 'missing JSON version field')
    parent[path[-1]] = value
    return json.dumps(data, indent=2, ensure_ascii=False) + '\n'


def package_version(text, value):
    match = re.search(r'(?ms)^\[package\]\s*\n(.*?)(?=^\[|\Z)', text)
    require(match, 'missing Cargo package section')
    section, count = re.subn(r'(?m)^(version\s*=\s*)"[^"]+"', lambda m: m[1] + json.dumps(value), match[1])
    require(count == 1, 'Cargo package must own exactly one version')
    return text[:match.start(1)] + section + text[match.end(1):]


def versions_at(commit):
    values = {}
    for app in release.APPS:
        path = f'apps/{app}/' + ('Cargo.toml' if app in ('core', 'cli') else 'app.json' if app == 'mobile' else 'package.json')
        text = blob(commit, path).decode()
        if app in ('core', 'cli'):
            match = re.search(r'(?ms)^\[package\]\s*\n(.*?)(?=^\[|\Z)', text)
            require(match, 'missing Cargo package')
            values[app] = re.search(r'(?m)^version\s*=\s*"([^"]+)"', match[1])[1]
        else:
            info = json.loads(text)
            values[app] = info['expo']['extra']['releaseVersion'] if app == 'mobile' else info['version']
        release.version(values[app])
    return values


def plan_versions(day, base, tags):
    datetime.datetime.strptime(day, '%Y%m%d')
    values = versions_at(base)
    for app, current in values.items():
        baseline = current.split('-', 1)[0]
        prefix = f'{baseline}-nightly.{day}.'
        existing = [current] + [tag[len(app) + 2:] for tag in tags if tag.startswith(app + '-v')]
        sequences = [int(value[len(prefix):]) for value in existing
                     if value.startswith(prefix) and re.fullmatch(r'[1-9][0-9]*', value[len(prefix):])]
        values[app] = prefix + str(max(sequences, default=0) + 1)
    expo = json.loads(blob(base, 'apps/mobile/app.json'))['expo']
    code = expo['android']['versionCode']
    require(type(code) is int and 0 < code < 2100000000, 'Android versionCode cannot be incremented')
    return DailyPlan(day, base, values, code + 1)


def generated(plan):
    old = versions_at(plan.base)
    require(set(plan.versions) == set(release.APPS), 'daily plan must contain all five apps')
    for app, value in plan.versions.items():
        release.version(value)
        prefix = old[app].split('-', 1)[0] + '-nightly.' + plan.day + '.'
        require(value.startswith(prefix), 'daily version changed baseline or date')
        if old[app].startswith(prefix):
            require(int(value[len(prefix):]) > int(old[app][len(prefix):]), 'daily version did not advance')
    result = {path: blob(plan.base, path).decode() for path in PATHS}
    for app in ('www', 'app'):
        path = f'apps/{app}/package.json'
        result[path] = json_value(result[path], ('version',), plan.versions[app])
    for app in ('core', 'cli'):
        path = f'apps/{app}/Cargo.toml'
        result[path] = package_version(result[path], plan.versions[app])
    blocks = result['Cargo.lock'].split('[[package]]')
    found = set()
    for index, block in enumerate(blocks):
        match = re.search(r'(?m)^name = "(vibeface|vibeface-core)"$', block)
        if match:
            app = 'cli' if match[1] == 'vibeface' else 'core'
            require(app not in found and f'version = "{old[app]}"' in block, 'Cargo.lock local package mismatch')
            blocks[index], count = re.subn(r'(?m)^version = "[^"]+"$', 'version = "' + plan.versions[app] + '"', block)
            require(count == 1, 'ambiguous local lock version')
            found.add(app)
    require(found == {'core', 'cli'}, 'Cargo.lock is missing local packages')
    result['Cargo.lock'] = '[[package]]'.join(blocks)
    lock_path = 'apps/cli/release-lock.json'
    lock = json.loads(result[lock_path])
    require(set(lock) == {'core', 'app'} and all(set(pin) == {'tag'} for pin in lock.values()), 'unsupported CLI lock')
    for app in ('core', 'app'):
        result[lock_path] = json_value(result[lock_path], (app, 'tag'), f'{app}-v{plan.versions[app]}')
    mobile_path = 'apps/mobile/app.json'
    expo = json.loads(result[mobile_path])['expo']
    require(expo['version'] == plan.versions['mobile'].split('-', 1)[0], 'mobile marketing version mismatch')
    require(type(plan.android_code) is int and plan.android_code == expo['android']['versionCode'] + 1
            and 0 < plan.android_code <= 2100000000, 'Android versionCode must advance exactly once')
    result[mobile_path] = json_value(result[mobile_path], ('expo', 'extra', 'releaseVersion'), plan.versions['mobile'])
    result[mobile_path] = json_value(result[mobile_path], ('expo', 'android', 'versionCode'), plan.android_code)
    return {path: text.encode() for path, text in result.items()}


def recovered_plan(day, head):
    parents = git('rev-list', '--parents', '-n', '1', head).split()
    require(len(parents) == 2, 'daily version head must be one ordinary commit')
    expo = json.loads(blob(head, 'apps/mobile/app.json'))['expo']
    return DailyPlan(day, parents[1], versions_at(head), expo['android']['versionCode'])


def expected_tree(plan):
    expected = generated(plan)
    with tempfile.TemporaryDirectory(dir=scratch(), prefix='index-') as temporary:
        environment = dict(os.environ, GIT_INDEX_FILE=str(Path(temporary) / 'index'))
        git('read-tree', plan.base, env=environment)
        for path, content in expected.items():
            entry = git('ls-tree', plan.base, '--', path).split()
            require(entry[:2] == ['100644', 'blob'], 'version source must be an ordinary tracked file')
            oid = git('hash-object', '-w', '--stdin', data=content)
            git('update-index', '--add', '--cacheinfo', '100644', oid, path, env=environment)
        return git('write-tree', env=environment)


def verify_tree(plan, head):
    require(git('show', '-s', '--format=%ae%n%ce', head).splitlines() == [BOT, BOT], 'unexpected daily commit author')
    require(git('rev-list', '--parents', '-n', '1', head).split() == [head, plan.base], 'daily version head changed its original base')
    require(expected_tree(plan) == git('rev-parse', head + '^{tree}'),
            'daily PR tree differs from exact mechanical version changes; refusing merge')


def scratch():
    directory = ROOT / '.artifacts/daily-nightly'
    directory.mkdir(parents=True, exist_ok=True)
    return directory


@contextmanager
def checkout(commit):
    with tempfile.TemporaryDirectory(dir=scratch(), prefix='checkout-') as temporary:
        source = Path(temporary) / 'source'
        git('worktree', 'add', '--detach', str(source), commit)
        try:
            yield source
        finally:
            git('worktree', 'remove', '--force', str(source))


def metadata(source, plan):
    before = (source / 'Cargo.lock').read_bytes()
    result = subprocess.check_output(['cargo', 'metadata', '--locked', '--no-deps', '--format-version', '1'], cwd=source)
    packages = {package['name']: package['version'] for package in json.loads(result)['packages']}
    require(packages['vibeface'] == plan.versions['cli'] and packages['vibeface-core'] == plan.versions['core'],
            'Cargo metadata does not match daily versions')
    require((source / 'Cargo.lock').read_bytes() == before, 'cargo metadata changed dependency resolution')


def daily_pr(branch):
    matches = pages('pulls?state=all&head=' + quote('leaperone:' + branch, safe=''))
    require(len(matches) <= 1, 'daily branch has multiple PRs; refusing a second daily PR')
    return api(f'pulls/{matches[0]["number"]}') if matches else None


def branch_sha(branch):
    ref = api('git/ref/heads/' + branch, missing=True)
    return ref['object']['sha'] if ref else None


def validate_pr(pr, plan, head, login):
    require(pr['user']['login'] == login and pr['head']['repo']['full_name'] == REPO
            and pr['base']['repo']['full_name'] == REPO and pr['base']['ref'] == 'main'
            and pr['head']['ref'] == plan.branch and pr['head']['sha'] == head
            and pr['commits'] == 1 and not pr['draft'] and pr['title'] == 'chore(release): daily nightly ' + plan.day,
            'daily PR identity, author, base or head mismatch')
    require(pr['state'] == 'open' or pr.get('merged'), 'daily PR was closed without merge; refusing a replacement')
    verify_tree(plan, head)


def create_pr(plan, head, login):
    def read():
        return daily_pr(plan.branch)
    pr = reconcile_write(lambda: api('pulls', 'POST', dict(head=plan.branch, base='main',
                         title='chore(release): daily nightly ' + plan.day,
                         body='Mechanical daily Nightly versions. Only the trusted daily coordinator may merge this PR.'), pat=True),
                         read, lambda value: value is not None, 'daily PR creation')
    validate_pr(pr, plan, head, login)
    return pr


def merge_pr(pr, plan, head, login):
    validate_pr(pr, plan, head, login)
    if not pr.get('merged'):
        git('fetch', 'origin', 'main')
        git('merge-base', '--is-ancestor', plan.base, 'origin/main')
        with checkout(head) as source:
            metadata(source, plan)
        for attempt in range(18):
            pr = api(f'pulls/{pr["number"]}')
            validate_pr(pr, plan, head, login)
            if pr['mergeable'] is not None:
                break
            if attempt < 17:
                time.sleep(10)
        require(pr['mergeable'] is True, 'daily PR is conflicted or mergeability is unknown; rerun after reconciliation')
        number = pr['number']
        pr = reconcile_write(lambda: api(f'pulls/{number}/merge', 'PUT', dict(sha=head, merge_method='squash'), pat=True),
                             lambda: api(f'pulls/{number}'), lambda value: bool(value.get('merged')), 'daily squash merge')
        validate_pr(pr, plan, head, login)
    source = pr['merge_commit_sha']
    require(re.fullmatch(r'[a-f0-9]{40}', source or ''), 'merged PR has no actual merge SHA')
    git('fetch', 'origin', 'main')
    git('merge-base', '--is-ancestor', source, 'origin/main')
    parents = git('rev-list', '--parents', '-n', '1', source).split()
    require(len(parents) == 2, 'daily squash merge must have one real main parent')
    landed = DailyPlan(plan.day, parents[1], plan.versions, plan.android_code)
    require(git('rev-parse', source + '^{tree}') == expected_tree(landed),
            'actual main merge differs from exact mechanical versions on its real parent; retain original PR and stop')
    return source


def ensure_landed(day, login):
    branch = 'bot/daily-nightly/' + day
    pr = daily_pr(branch)
    remote = branch_sha(branch)
    if pr:
        head = pr['head']['sha']
        require(remote is None and pr.get('merged') or remote == head, 'daily branch no longer matches original PR head')
        git('fetch', 'origin', f'pull/{pr["number"]}/head')
        plan = recovered_plan(day, head)
    elif remote:
        git('fetch', 'origin', 'refs/heads/' + branch)
        head = remote
        plan = recovered_plan(day, head)
        verify_tree(plan, head)
        pr = create_pr(plan, head, login)
    else:
        git('fetch', 'origin', 'main', '--tags')
        base = git('rev-parse', 'origin/main')
        tags = [line.split('\t')[1][10:] for line in git('ls-remote', '--tags', 'origin').splitlines()
                if not line.endswith('^{}')]
        plan = plan_versions(day, base, tags)
        with checkout(base) as source:
            for path, content in generated(plan).items():
                (source / path).write_bytes(content)
            metadata(source, plan)
            git('add', '--', *PATHS, root=source)
            git('-c', 'user.name=github-actions[bot]', '-c', 'user.email=' + BOT,
                'commit', '-m', 'chore(release): daily nightly ' + day, root=source)
            head = git('rev-parse', 'HEAD', root=source)
            verify_tree(plan, head)
            reconcile_write(lambda: git('push', 'origin', head + ':refs/heads/' + branch),
                            lambda: branch_sha(branch), lambda value: value == head, 'daily branch push')
        pr = create_pr(plan, head, login)
    return plan, merge_pr(pr, plan, head, login), pr['number']


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


def wait_main_ci(source, deadline):
    restored = set()
    while time.monotonic() < deadline:
        runs = matching_runs(CI, source, 'main', {'push'})
        require(len(runs) <= 1, 'multiple main push CI runs for daily merge; reconcile before releasing')
        if runs:
            run = runs[0]
            if run['status'] == 'completed':
                if run['conclusion'] == 'success':
                    return run
                require(run['conclusion'] == 'cancelled' and run['id'] not in restored,
                        'exact daily merge main CI did not succeed; refusing tags')
                restored.add(run['id'])
                rerun(run, False)
        time.sleep(15)
    raise ValueError('exact daily merge main push CI is missing or pending; refusing tags')


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
        existing = reconcile_write(lambda: api('git/refs', 'POST', dict(ref='refs/tags/' + tag, sha=source)),
                                   lambda: tag_sha(tag), lambda value: value is not None, 'release tag creation')
    require(existing == source, 'existing immutable tag points to a different SHA')


def release_state(tag):
    return api('releases/tags/' + tag, missing=True)


def start_release(app, source):
    tag = release.identity(app)['tag']
    ensure_tag(tag, source)
    state = release_state(tag)
    runs = matching_runs(RELEASE, source, tag, {'push', 'workflow_dispatch'})
    if state and not state['draft']:
        return runs
    if not runs:
        require(state is None, 'draft release has no original run; original bytes cannot be recovered')
        runs = reconcile_write(lambda: api('actions/workflows/release.yml/dispatches', 'POST', dict(ref=tag, inputs=dict(tag=tag))),
                               lambda: matching_runs(RELEASE, source, tag, {'push', 'workflow_dispatch'}), bool, 'release workflow dispatch')
    return runs


def recover_publisher(app, run, source):
    artifacts = pages(f'actions/runs/{run["id"]}/artifacts', 'artifacts')
    expected = {'static-release'} if app in ('www', 'app') else {f'release-android-{source}'} if app == 'mobile' else {'native-' + target for target in release.TARGETS}
    available = [artifact['name'] for artifact in artifacts if not artifact['expired'] and artifact['size_in_bytes'] > 0]
    require(all(available.count(name) == 1 for name in expected), 'original complete release artifact is unavailable; refusing a rebuild')
    jobs = pages(f'actions/runs/{run["id"]}/jobs?filter=latest', 'jobs')
    failed = [job for job in jobs if job['conclusion'] not in ('success', 'skipped')]
    publisher = 'publish-static' if app in ('www', 'app') else 'publish-android' if app == 'mobile' else 'publish-native'
    require(failed and all(job['status'] == 'completed' and job['name'].split(' / ')[-1] == publisher for job in failed),
            'failure includes a build job; only an original-artifact publisher may be recovered')
    return rerun(run, True)


def finish_release(app, source, deadline):
    tag = release.identity(app)['tag']
    restored = set()
    while time.monotonic() < deadline:
        require(tag_sha(tag) == source, 'release tag changed source')
        runs = matching_runs(RELEASE, source, tag, {'push', 'workflow_dispatch'})
        state = release_state(tag)
        active = [run for run in runs if run['status'] != 'completed']
        if not active:
            if state and not state['draft']:
                with tempfile.TemporaryDirectory(dir=scratch(), prefix='public-') as temporary:
                    release.verify_public(app, Path(temporary) / 'assets')
                return [dict(id=run['id'], url=run['html_url'], conclusion=run['conclusion']) for run in runs]
            require(runs, 'release has no original workflow run')
            require(len(runs) == 1, 'multiple unsuccessful release runs; reconcile original artifacts manually')
            run = runs[0]
            require(run['conclusion'] != 'success' and run['id'] not in restored, 'release is not public after original workflow completion')
            restored.add(run['id'])
            recover_publisher(app, run, source)
        time.sleep(15)
    raise ValueError('release workflow is still pending; rerun the original daily coordinator to reconcile')


def publish_daily(source, deadline):
    receipt = {}; started = []; errors = []
    for app in ('www', 'app', 'core', 'mobile'):
        started.append(app)
        try:
            start_release(app, source)
        except Exception as error:
            errors.append(f'{app}: {error}')
            break
    for app in ('core', 'app'):
        if app in started:
            try:
                receipt[app] = finish_release(app, source, deadline)
            except Exception as error:
                errors.append(f'{app}: {error}')
    if not errors and {'core', 'app'} <= set(receipt):
        started.append('cli')
        try:
            start_release('cli', source)
        except Exception as error:
            errors.append(f'cli: {error}')
    for app in started:
        if app not in ('core', 'app'):
            try:
                receipt[app] = finish_release(app, source, deadline)
            except Exception as error:
                errors.append(f'{app}: {error}')
    for app in started:
        tag = release.identity(app)['tag']
        runs = matching_runs(RELEASE, source, tag, {'push', 'workflow_dispatch'})
        while any(run['status'] != 'completed' for run in runs) and time.monotonic() < deadline:
            time.sleep(15)
            runs = matching_runs(RELEASE, source, tag, {'push', 'workflow_dispatch'})
        print(json.dumps(dict(app=app, runs=[dict(id=run['id'], url=run['html_url'], status=run['status'],
                                                conclusion=run['conclusion']) for run in runs])))
        if any(run['status'] != 'completed' for run in runs):
            errors.append(f'{app}: associated runs remain pending')
    require(not errors, '; '.join(errors))
    return receipt


def run_daily(run_id):
    require(os.environ.get('NIGHTLY_RELEASE_TOKEN'), 'NIGHTLY_RELEASE_TOKEN is missing; daily Nightly has not been enabled')
    require(os.environ.get('GH_TOKEN'), 'GITHUB_TOKEN must be supplied as GH_TOKEN for branch/tag/dispatch writes')
    require(os.environ.get('GITHUB_REPOSITORY') == REPO and os.environ.get('GITHUB_EVENT_NAME') == 'schedule'
            and os.environ.get('GITHUB_REF') == 'refs/heads/main' and str(run_id) == os.environ.get('GITHUB_RUN_ID'),
            'daily coordinator only accepts its original main schedule run')
    original = api(f'actions/runs/{run_id}')
    require(original['event'] == 'schedule' and original['head_branch'] == 'main' and original['path'] == CI
            and original['head_sha'] == os.environ.get('GITHUB_SHA') and original['repository']['full_name'] == REPO,
            'untrusted daily schedule source')
    day = datetime.datetime.fromisoformat(original['created_at'].replace('Z', '+00:00')).astimezone(datetime.timezone.utc).strftime('%Y%m%d')
    login = api('/user', pat=True)['login']
    deadline = time.monotonic() + 5 * 60 * 60
    plan, source, number = ensure_landed(day, login)
    ci = wait_main_ci(source, deadline)
    with checkout(source) as clean:
        previous = release.ROOT
        release.ROOT = clean
        try:
            require(versions_at(source) == plan.versions and json.loads((clean / 'apps/cli/release-lock.json').read_text()) == {
                app: {'tag': f'{app}-v{plan.versions[app]}'} for app in ('core', 'app')}, 'landed daily versions or CLI pins changed')
            for app in release.APPS:
                environment = {key: value for key, value in os.environ.items()
                               if key not in ('GITHUB_EVENT_NAME', 'GITHUB_REF', 'NIGHTLY_RELEASE_TOKEN')}
                subprocess.run(['python3', str(clean / 'scripts/release.py'), 'plan', '--tag', release.identity(app)['tag'], '--require-main'],
                               cwd=clean, env=environment, check=True)
            releases = publish_daily(source, deadline)
        finally:
            release.ROOT = previous
    print(json.dumps(dict(day=day, pr=number, source_commit=source, versions=plan.versions,
                          main_ci=ci['html_url'], releases=releases), indent=2))


def self_test():
    from unittest.mock import patch

    def rejected(call, message=None):
        try:
            call()
        except ValueError as error:
            assert message is None or message in str(error), str(error)
        else:
            raise AssertionError('unsafe operation was accepted')

    with tempfile.TemporaryDirectory(dir=scratch(), prefix='self-test-') as temporary:
        root = Path(temporary) / 'repo'; root.mkdir()
        for app in release.APPS:
            directory = root / 'apps' / app; directory.mkdir(parents=True)
            if app in ('core', 'cli'):
                (directory / 'src').mkdir(); (directory / 'src/main.rs').write_text('fn main() {}\n')
                (directory / 'Cargo.toml').write_text('[package]\nname = "' + ('vibeface' if app == 'cli' else 'vibeface-core')
                                                     + '"\nversion = "0.0.1-nightly.20261105.2"\nedition = "2021"\n')
            else:
                value = {'version': '0.0.1'} if app == 'www' else {'version': '0.0.1-nightly.20261105.2', 'private': True}
                if app == 'mobile':
                    value = {'expo': {'version': '0.0.1', 'ios': {'buildNumber': '99'},
                        'android': {'package': 'one.leaper.vibeface', 'versionCode': 250001},
                        'extra': {'releaseVersion': '0.0.1-nightly.20261105.1'}}}
                (directory / ('app.json' if app == 'mobile' else 'package.json')).write_text(json.dumps(value, indent=2) + '\n')
        (root / 'apps/cli/release-lock.json').write_text(json.dumps({app: {'tag': f'{app}-v0.0.1-nightly.20261105.2'} for app in ('core', 'app')}, indent=2) + '\n')
        (root / 'Cargo.toml').write_text('[workspace]\nmembers = ["apps/core", "apps/cli"]\nresolver = "2"\n')
        (root / 'Cargo.lock').write_text('version = 3\n\n[[package]]\nname = "vibeface"\nversion = "0.0.1-nightly.20261105.2"\n\n[[package]]\nname = "vibeface-core"\nversion = "0.0.1-nightly.20261105.2"\n')
        git('init', '--initial-branch=main', root=root)
        git('add', '.', root=root)
        git('-c', 'user.name=github-actions[bot]', '-c', 'user.email=' + BOT, 'commit', '-qm', 'fixture', root=root)
        with patch(f'{__name__}.ROOT', root), patch('release.ROOT', root), patch(f'{__name__}.time.sleep'):
            base = git('rev-parse', 'HEAD')
            plan = plan_versions('20261106', base, ['core-v0.0.1-nightly.20261106.7'])
            assert plan.versions['core'] == '0.0.1-nightly.20261106.8'
            assert plan.versions['www'] == '0.0.1-nightly.20261106.1'
            assert plan_versions('20261105', base, []).versions['core'].endswith('.3')
            contents = generated(plan)
            assert json.loads(contents['apps/mobile/app.json'])['expo']['ios']['buildNumber'] == '99'
            assert contents['Cargo.lock'].count(b'[[package]]') == 2

            def commit_files(changes):
                git('checkout', '--detach', '-q', base)
                for path, content in contents.items():
                    (root / path).write_bytes(content)
                for path, content in changes.items():
                    (root / path).write_bytes(content)
                git('add', '--all')
                git('-c', 'user.name=github-actions[bot]', '-c', 'user.email=' + BOT, 'commit', '-qm', 'daily fixture')
                return git('rev-parse', 'HEAD')

            head = commit_files({})
            assert recovered_plan(plan.day, head) == plan
            verify_tree(plan, head)
            metadata(root, plan)
            for path, content in (
                ('apps/app/package.json', contents['apps/app/package.json'].replace(b'"private": true', b'"private": false')),
                ('Cargo.lock', contents['Cargo.lock'] + b'\n[[package]]\nname = "unexpected"\nversion = "1.0.0"\n'),
                ('apps/core/Cargo.toml', contents['apps/core/Cargo.toml'] + b'\n[dependencies]\nserde = "1"\n'),
                ('apps/cli/release-lock.json', contents['apps/cli/release-lock.json'].replace(b'core-v', b'app-v')),
                ('apps/mobile/app.json', contents['apps/mobile/app.json'].replace(b'250002', b'1')),
                ('extra.txt', b'not a version file\n'),
            ):
                bad = commit_files({path: content})
                rejected(lambda: verify_tree(plan, bad), 'mechanical')
                git('checkout', '--detach', '-q', base)
            head = commit_files({})
            git('checkout', '--detach', '-q', base)
            (root / 'business.txt').write_text('main advanced independently\n')
            git('add', 'business.txt')
            git('-c', 'user.name=github-actions[bot]', '-c', 'user.email=' + BOT, 'commit', '-qm', 'independent main change')
            advanced = git('rev-parse', 'HEAD')
            landed = DailyPlan(plan.day, advanced, plan.versions, plan.android_code)
            merged = git('-c', 'user.name=github-actions[bot]', '-c', 'user.email=' + BOT,
                         'commit-tree', expected_tree(landed), '-p', advanced, '-m', 'actual squash fixture')
            assert merged != head
            assert blob(merged, 'business.txt') == b'main advanced independently\n'
            git('update-ref', 'refs/remotes/origin/main', advanced)
            pr = dict(number=17, user=dict(login='nightly-owner'), head=dict(repo=dict(full_name=REPO), ref=plan.branch, sha=head),
                      base=dict(repo=dict(full_name=REPO), ref='main'), commits=1, draft=False,
                      title='chore(release): daily nightly ' + plan.day, state='open', merged=False, mergeable=True)
            state = {'pr': None, 'merge_checks': 0}; writes = []
            real_git = git

            def fixture_git(*args, **kwargs):
                return '' if args[0] == 'fetch' else real_git(*args, **kwargs)

            def pr_api(path, method='GET', data=None, pat=False, missing=False):
                if method == 'POST':
                    assert path == 'pulls' and pat
                    writes.append((path, method, data)); state['pr'] = dict(pr)
                    raise ValueError('PR created but response lost')
                if method == 'PUT':
                    assert path == 'pulls/17/merge' and pat and data == dict(sha=head, merge_method='squash')
                    assert state['merge_checks'] >= 2
                    writes.append((path, method, data))
                    state['pr'] = dict(pr, merged=True, state='closed', merge_commit_sha=merged)
                    real_git('update-ref', 'refs/remotes/origin/main', merged)
                    raise ValueError('merge completed but response lost')
                assert path == 'pulls/17'
                if not state['pr'].get('merged'):
                    state['merge_checks'] += 1
                    if state['merge_checks'] == 1:
                        return dict(state['pr'], mergeable=None)
                return state['pr']

            with patch(f'{__name__}.api', side_effect=pr_api), patch(f'{__name__}.daily_pr', side_effect=lambda branch: state['pr']), \
                    patch(f'{__name__}.git', side_effect=fixture_git):
                created = create_pr(plan, head, 'nightly-owner')
                assert merge_pr(created, plan, head, 'nightly-owner') == merged
                assert len(writes) == 2
                assert merge_pr(state['pr'], plan, head, 'nightly-owner') == merged and len(writes) == 2
                with patch(f'{__name__}.branch_sha', return_value=head), patch(f'{__name__}.plan_versions', side_effect=AssertionError('same-day reallocation')):
                    assert ensure_landed(plan.day, 'nightly-owner') == (plan, merged, 17)
                rejected(lambda: validate_pr(dict(pr, user=dict(login='outsider')), plan, head, 'nightly-owner'), 'identity')
            next_day = plan_versions('20261107', merged, [])
            assert all('-nightly.20261107.1' in value for value in next_day.versions.values())
            assert next_day.android_code == plan.android_code + 1

            good = dict(id=1, run_attempt=1, head_sha=merged, head_branch='main', event='push', path=CI,
                        repository=dict(full_name=REPO), head_repository=dict(full_name=REPO),
                        status='completed', conclusion='success', html_url='https://example.invalid/main-ci')
            with patch(f'{__name__}.pages', return_value=[good]):
                assert wait_main_ci(merged, time.monotonic() + 1)['id'] == 1
            for change in ({'head_sha': 'b' * 40}, {'event': 'workflow_dispatch'}, {'head_branch': 'feature'}, {'path': RELEASE},
                           {'status': 'in_progress', 'conclusion': None}, {'conclusion': 'failure'}):
                with patch(f'{__name__}.pages', return_value=[dict(good, **change)]), \
                        patch(f'{__name__}.time.monotonic', side_effect=[0, 1]), patch(f'{__name__}.api') as no_write:
                    rejected(lambda: wait_main_ci(merged, 0.5))
                    no_write.assert_not_called()
            cancelled = {'run': dict(good, conclusion='cancelled')}; writes = []
            def cancelled_api(path, method='GET', **kwargs):
                if method == 'POST':
                    writes.append(path); cancelled['run'] = dict(good, run_attempt=2)
                    raise ValueError('CI recovery response lost')
                return cancelled['run']
            with patch(f'{__name__}.pages', side_effect=lambda *args: [cancelled['run']]), \
                    patch(f'{__name__}.api', side_effect=cancelled_api):
                assert wait_main_ci(merged, time.monotonic() + 1)['run_attempt'] == 2
                assert writes == ['actions/runs/1/rerun']

            writes = []; remote = {'tag': None, 'runs': [], 'public': False}
            tag = 'app-v' + plan.versions['app']

            def release_api(path, method='GET', data=None, **kwargs):
                if method != 'GET':
                    writes.append((path, method, data))
                    if path == 'git/refs':
                        remote['tag'] = data['sha']
                    elif path.endswith('/dispatches'):
                        remote['runs'] = [dict(good, head_branch=tag, event='workflow_dispatch', path=RELEASE)]
                    else:
                        raise AssertionError('unexpected write')
                    raise ValueError('write accepted but response lost')
                if path.startswith('git/ref/tags/'):
                    return dict(object=dict(type='commit', sha=remote['tag'])) if remote['tag'] else None
                assert path.startswith('releases/tags/')
                return dict(draft=False) if remote['public'] else None

            with patch(f'{__name__}.api', side_effect=release_api), \
                    patch(f'{__name__}.matching_runs', side_effect=lambda *args: remote['runs']), \
                    patch('release.identity', return_value=dict(tag=tag)):
                ensure_tag(tag, merged); ensure_tag(tag, merged)
                assert len(writes) == 1
                assert start_release('app', merged) and len(writes) == 2
                assert start_release('app', merged) and len(writes) == 2
                remote['public'] = True; remote['runs'] = []
                assert start_release('app', merged) == [] and len(writes) == 2
                remote['tag'] = 'b' * 40
                rejected(lambda: start_release('app', merged), 'different SHA')
                assert len(writes) == 2
            attempts = {'value': dict(good, conclusion='failure')}; writes = []

            def recovery_api(path, method='GET', **kwargs):
                if method == 'POST':
                    writes.append(path)
                    attempts['value'] = dict(good, run_attempt=2, status='queued', conclusion=None)
                    raise ValueError('rerun response lost')
                return attempts['value']

            def recovery_pages(path, key):
                return [dict(name='static-release', expired=False, size_in_bytes=100)] if key == 'artifacts' else [dict(name='publish-static', status='completed', conclusion='failure')]

            with patch(f'{__name__}.api', side_effect=recovery_api), patch(f'{__name__}.pages', side_effect=recovery_pages):
                assert recover_publisher('app', dict(good, conclusion='failure'), merged)['run_attempt'] == 2
                assert writes == ['actions/runs/1/rerun-failed-jobs']
                with patch(f'{__name__}.pages', return_value=[]):
                    rejected(lambda: recover_publisher('app', good, merged), 'artifact is unavailable')
                assert len(writes) == 1

            for blocked in ('core', 'app', None):
                operations = []
                def finish(app, *args):
                    operations.append(('verified', app))
                    require(app != blocked, 'component is not public or verified')
                    return []
                with patch(f'{__name__}.start_release', side_effect=lambda app, *args: operations.append(('started', app))), \
                        patch(f'{__name__}.finish_release', side_effect=finish), patch(f'{__name__}.matching_runs', return_value=[]):
                    if blocked:
                        rejected(lambda: publish_daily(merged, 1), 'not public')
                        assert ('started', 'cli') not in operations
                    else:
                        assert set(publish_daily(merged, 1)) == set(release.APPS)
                        assert operations.index(('started', 'cli')) > operations.index(('verified', 'core'))
                        assert operations.index(('started', 'cli')) > operations.index(('verified', 'app'))
            observed = []
            def drain(*args):
                observed.append(args[2])
                return [dict(good, status='in_progress', conclusion=None)] if len(observed) == 1 else [good]
            with patch(f'{__name__}.start_release'), patch(f'{__name__}.finish_release', side_effect=ValueError('component failed')), \
                    patch(f'{__name__}.matching_runs', side_effect=drain), patch(f'{__name__}.time.monotonic', return_value=0):
                rejected(lambda: publish_daily(merged, 1), 'component failed')
                assert len(observed) == 5 and observed[0] == observed[1]
                assert all(any(tag.startswith(app + '-v') for tag in observed) for app in ('www', 'app', 'core', 'mobile'))
                assert not any(tag.startswith('cli-v') for tag in observed)

            environment = dict(GITHUB_REPOSITORY=REPO, GITHUB_EVENT_NAME='schedule', GITHUB_REF='refs/heads/main',
                               GITHUB_RUN_ID='42', GITHUB_SHA=merged, NIGHTLY_RELEASE_TOKEN='fixture-only', GH_TOKEN='fixture-only')
            original = dict(good, event='schedule', created_at='2026-11-06T23:59:59Z')
            dates = []
            def observe_day(day, login):
                dates.append(day)
                raise ValueError('date recovered from original run')
            with patch.dict(os.environ, environment), patch(f'{__name__}.api', side_effect=lambda path, **kwargs: dict(login='owner') if path == '/user' else original), \
                    patch(f'{__name__}.ensure_landed', side_effect=observe_day):
                rejected(lambda: run_daily(42), 'date recovered')
                assert dates == ['20261106']
            for changes in ({'NIGHTLY_RELEASE_TOKEN': ''}, {'GITHUB_EVENT_NAME': 'workflow_dispatch'}):
                with patch.dict(os.environ, dict(environment, **changes)), patch(f'{__name__}.api') as no_write:
                    rejected(lambda: run_daily(42))
                    no_write.assert_not_called()

    workflow = (ROOT / CI).read_text()
    assert "cron: '23 3 * * *'" in workflow and 'group: daily-nightly\n      cancel-in-progress: false' in workflow
    assert "if: github.event_name == 'schedule' && github.ref == 'refs/heads/main' && needs.ci-gate.result == 'success'" in workflow
    static = (ROOT / RELEASE).read_text()
    android = (ROOT / '.github/workflows/release-apps.yml').read_text()
    assert 'needs: [identity, publish-static]' in static and 'name: static-release' in static
    assert android.index('python3 scripts/release.py manifest mobile') < android.index('actions/upload-artifact@')
    assert 'needs: build-android' in android and 'path: .artifacts/android/*' in android
    print('daily Nightly real-Git, identity, reconciliation and original-artifact self-tests passed')


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
