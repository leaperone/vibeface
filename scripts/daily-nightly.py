#!/usr/bin/env python3
"""Land one mechanical version PR per four-hour UTC batch, then reconcile both Nightlies."""
import argparse
from contextlib import contextmanager
from dataclasses import dataclass
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

ROOT = Path(__file__).resolve().parent.parent
REPO = 'leaperone/vibeface'
CI = '.github/workflows/ci.yml'
RELEASE = '.github/workflows/release.yml'
PATHS = ('apps/www/package.json', 'apps/app/package.json', 'apps/core/Cargo.toml',
         'apps/cli/Cargo.toml', 'Cargo.lock', 'apps/mobile/app.json', 'apps/desktop/package.json')
PRODUCT_APPS = release.PRODUCT_APPS
RELEASES = release.RELEASES
PRODUCER = 'collect'
PUBLISHERS = ('publish', 'npm')
BOT = '41898282+github-actions[bot]@users.noreply.github.com'
require = release.require


@dataclass(frozen=True)
class DailyPlan:
    batch: str
    base: str
    versions: dict
    android_code: int
    ios_build_number: str

    def __post_init__(self):
        slot = datetime.datetime.strptime(self.batch, '%Y%m%d%H')
        require(slot.strftime('%Y%m%d%H') == self.batch and slot.hour % 4 == 0,
                'Nightly batch must be a four-hour UTC slot')

    @property
    def day(self):
        return self.batch[:8]

    @property
    def branch(self):
        return 'bot/daily-nightly/' + self.batch


def schedule_batch(created_at):
    created = datetime.datetime.fromisoformat(created_at.replace('Z', '+00:00')).astimezone(datetime.timezone.utc)
    return created.replace(hour=created.hour // 4 * 4).strftime('%Y%m%d%H')


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


def next_ios_build_number(value):
    """Increment the last valid CFBundleVersion component without rollover."""
    require(isinstance(value, str), 'iOS build number must be a string')
    parts = value.split('.')
    require(1 <= len(parts) <= 3 and re.fullmatch(r'[0-9]{1,4}', parts[0])
            and int(parts[0]) > 0 and all(re.fullmatch(r'[0-9]{1,2}', part) for part in parts[1:]),
            'iOS build number must be a valid Apple CFBundleVersion')
    maximum = 9999 if len(parts) == 1 else 99
    last = int(parts[-1])
    require(last < maximum, 'iOS build number component cannot be incremented without rollover')
    parts[-1] = str(last + 1)
    return '.'.join(parts)


def plan_versions(batch, base, tags):
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
    expo = json.loads(blob(base, 'apps/mobile/app.json'))['expo']
    code = expo['android']['versionCode']
    require(type(code) is int and 0 < code < 2100000000, 'Android versionCode cannot be incremented')
    ios_build_number = next_ios_build_number(expo['ios']['buildNumber'])
    return DailyPlan(batch, base, values, code + 1, ios_build_number)


def generated(plan):
    old = versions_at(plan.base)
    require(set(plan.versions) == set(release.APPS), 'daily plan must contain all release applications')
    product_ver = plan.versions['cli']
    for app in PRODUCT_APPS:
        require(plan.versions[app] == product_ver, 'product apps must share one canonical version')
    for app, value in plan.versions.items():
        release.version(value)
        prefix = old[app].split('-', 1)[0] + '-nightly.' + plan.day + '.'
        require(value.startswith(prefix), 'daily version changed baseline or date')
        if old[app].startswith(prefix):
            require(int(value[len(prefix):]) > int(old[app][len(prefix):]), 'daily version did not advance')
    result = {path: blob(plan.base, path).decode() for path in PATHS}
    for app in ('www', 'app', 'desktop'):
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
    mobile_path = 'apps/mobile/app.json'
    expo = json.loads(result[mobile_path])['expo']
    require(expo['version'] == plan.versions['mobile'].split('-', 1)[0], 'mobile marketing version mismatch')
    require(type(plan.android_code) is int and plan.android_code == expo['android']['versionCode'] + 1
            and 0 < plan.android_code <= 2100000000, 'Android versionCode must advance exactly once')
    require(plan.ios_build_number == next_ios_build_number(expo['ios']['buildNumber']),
            'iOS build number must advance exactly once')
    result[mobile_path] = json_value(result[mobile_path], ('expo', 'extra', 'releaseVersion'), plan.versions['mobile'])
    result[mobile_path] = json_value(result[mobile_path], ('expo', 'android', 'versionCode'), plan.android_code)
    result[mobile_path] = json_value(result[mobile_path], ('expo', 'ios', 'buildNumber'), plan.ios_build_number)
    return {path: text.encode() for path, text in result.items()}


def recovered_plan(batch, head):
    parents = git('rev-list', '--parents', '-n', '1', head).split()
    require(len(parents) == 2, 'daily version head must be one ordinary commit')
    expo = json.loads(blob(head, 'apps/mobile/app.json'))['expo']
    return DailyPlan(batch, parents[1], versions_at(head), expo['android']['versionCode'], expo['ios']['buildNumber'])


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
            and pr['commits'] == 1 and not pr['draft'] and pr['title'] == 'chore(release): nightly ' + plan.batch,
            'daily PR identity, author, base or head mismatch')
    require(pr['state'] == 'open' or pr.get('merged'), 'daily PR was closed without merge; refusing a replacement')
    verify_tree(plan, head)


def create_pr(plan, head, login):
    def read():
        return daily_pr(plan.branch)
    pr = reconcile_write(lambda: api('pulls', 'POST', dict(head=plan.branch, base='main',
                         title='chore(release): nightly ' + plan.batch,
                         body='Mechanical four-hour Nightly versions. Only the trusted Nightly coordinator may merge this PR.'), pat=True),
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
    landed = DailyPlan(plan.batch, parents[1], plan.versions, plan.android_code, plan.ios_build_number)
    require(git('rev-parse', source + '^{tree}') == expected_tree(landed),
            'actual main merge differs from exact mechanical versions on its real parent; retain original PR and stop')
    return source


def ensure_landed(batch, login):
    branch = 'bot/daily-nightly/' + batch
    pr = daily_pr(branch)
    remote = branch_sha(branch)
    if pr:
        head = pr['head']['sha']
        require(remote is None and pr.get('merged') or remote == head, 'daily branch no longer matches original PR head')
        git('fetch', 'origin', f'pull/{pr["number"]}/head')
        plan = recovered_plan(batch, head)
    elif remote:
        git('fetch', 'origin', 'refs/heads/' + branch)
        head = remote
        plan = recovered_plan(batch, head)
        verify_tree(plan, head)
        pr = create_pr(plan, head, login)
    else:
        git('fetch', 'origin', 'main', '--tags')
        base = git('rev-parse', 'origin/main')
        tags = [line.split('\t')[1][10:] for line in git('ls-remote', '--tags', 'origin').splitlines()
                if not line.endswith('^{}')]
        plan = plan_versions(batch, base, tags)
        with checkout(base) as source:
            for path, content in generated(plan).items():
                (source / path).write_bytes(content)
            metadata(source, plan)
            git('add', '--', *PATHS, root=source)
            git('-c', 'user.name=github-actions[bot]', '-c', 'user.email=' + BOT,
                'commit', '-m', 'chore(release): nightly ' + batch, root=source)
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


def release_dispatch_tag(app):
    """Daily Nightly publishes exactly two groups: product v<version> and www www-v<version>."""
    require(app in RELEASES, 'daily Nightly publishes only the product and www release groups')
    return release.identity(app)['tag']


def start_release(app, source):
    tag = release_dispatch_tag(app)
    ensure_tag(tag, source)
    state = release_state(tag)
    runs = matching_runs(RELEASE, source, tag, {'push', 'workflow_dispatch'})
    if state and not state['draft']:
        return runs
    if not runs:
        require(state is None, 'draft release has no original run; original bytes cannot be recovered')
        # The release workflow only builds unless publish is requested explicitly.
        inputs = dict(app=app, tag=tag, publish='true')
        runs = reconcile_write(lambda: api('actions/workflows/release.yml/dispatches', 'POST', dict(ref=tag, inputs=inputs)),
                               lambda: matching_runs(RELEASE, source, tag, {'push', 'workflow_dispatch'}), bool, 'release workflow dispatch')
    return runs


def job_leaf(job):
    return job['name'].split(' / ')[-1]


def original_artifacts(app, run, source):
    """Immutable artifact IDs the publisher jobs must reuse instead of rebuilding."""
    prefixes = [f'{app}-release-{source}-']
    if app == 'product':
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
                'original complete release artifact is unavailable; refusing a rebuild')
        identifiers[prefix] = matches[0]['id']
    return identifiers


def recover_publisher(app, run, source):
    require(app in RELEASES, 'only the product and www release groups can be recovered')
    jobs = pages(f'actions/runs/{run["id"]}/jobs?filter=latest', 'jobs')
    producers = [job for job in jobs if job_leaf(job) == PRODUCER]
    require(len(producers) == 1 and producers[0]['conclusion'] == 'success',
            'the complete release artifact producer did not succeed; refusing a publisher recovery')
    failed = [job for job in jobs if job['conclusion'] not in ('success', 'skipped')]
    require(failed and all(job['status'] == 'completed' and job_leaf(job) in PUBLISHERS for job in failed),
            'failure includes a producer job; only an original-artifact publisher may be recovered')
    original_artifacts(app, run, source)
    return rerun(run, True)


def finish_release(app, source, deadline):
    tag = release_dispatch_tag(app)
    restored = set()
    while time.monotonic() < deadline:
        require(tag_sha(tag) == source, 'release tag changed source')
        runs = matching_runs(RELEASE, source, tag, {'push', 'workflow_dispatch'})
        state = release_state(tag)
        active = [run for run in runs if run['status'] != 'completed']
        if not active:
            require(runs, 'release has no original workflow run')
            require(len(runs) == 1, 'multiple release runs for one tag; reconcile original artifacts manually')
            run = runs[0]
            # A public Release with a failed npm job is not a success; it is a recoverable publisher failure.
            if state and not state['draft'] and run['conclusion'] == 'success':
                with tempfile.TemporaryDirectory(dir=scratch(), prefix='public-') as temporary:
                    release.verify_public(app, Path(temporary) / 'assets')
                return [dict(id=run['id'], url=run['html_url'], conclusion=run['conclusion'])]
            require(run['id'] not in restored, 'release is not complete after original workflow completion')
            restored.add(run['id'])
            recover_publisher(app, run, source)
        time.sleep(15)
    raise ValueError('release workflow is still pending; rerun the original daily coordinator to reconcile')


def publish_daily(source, deadline):
    receipt = {}; started = []; errors = []
    for app in RELEASES:
        started.append(app)
        try:
            start_release(app, source)
        except Exception as error:
            errors.append(f'{app}: {error}')
            break
    if not errors:
        for app in RELEASES:
            if app not in started:
                continue
            try:
                receipt[app] = finish_release(app, source, deadline)
            except Exception as error:
                errors.append(f'{app}: {error}')
    for app in started:
        tag = release_dispatch_tag(app)
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
    batch = schedule_batch(original['created_at'])
    login = api('/user', pat=True)['login']
    deadline = time.monotonic() + 5 * 60 * 60
    plan, source, number = ensure_landed(batch, login)
    ci = wait_main_ci(source, deadline)
    with checkout(source) as clean:
        previous = release.ROOT
        release.ROOT = clean
        try:
            require(versions_at(source) == plan.versions, 'landed daily versions changed')
            environment = {key: value for key, value in os.environ.items()
                           if key not in ('GITHUB_EVENT_NAME', 'GITHUB_REF', 'NIGHTLY_RELEASE_TOKEN')}
            environment['PYTHONDONTWRITEBYTECODE'] = '1'
            for app in RELEASES:
                tag = release.release_tag_for(app, plan.versions['cli' if app == 'product' else 'www'])
                subprocess.run(['python3', str(clean / 'scripts/release.py'), 'plan', '--app', app, '--tag', tag, '--require-main'],
                               cwd=clean, env=environment, check=True)
            releases = publish_daily(source, deadline)
        finally:
            release.ROOT = previous
    print(json.dumps(dict(batch=batch, day=plan.day, pr=number, source_commit=source, versions=plan.versions,
                          main_ci=ci['html_url'], releases=releases), indent=2))


def self_test():
    from unittest.mock import patch

    with patch.dict(os.environ, {'GH_TOKEN': 'fixture-actions-token', 'NIGHTLY_RELEASE_TOKEN': 'fixture-pr-token'}):
        with patch('subprocess.run', return_value=subprocess.CompletedProcess([], 0, '{}', '')) as transport:
            api('git/refs', 'POST', dict(ref='refs/tags/v0.0.1', sha='a' * 40))
            assert transport.call_args.kwargs['env']['GH_TOKEN'] == 'fixture-actions-token'
            api('pulls', 'POST', {}, pat=True)
            assert transport.call_args.kwargs['env']['GH_TOKEN'] == 'fixture-pr-token'

    def rejected(call, message=None):
        try:
            call()
        except ValueError as error:
            assert message is None or message in str(error), str(error)
        else:
            raise AssertionError('unsafe operation was accepted')

    for hour in range(24):
        assert schedule_batch(f'2026-11-06T{hour:02}:23:00Z') == f'20261106{hour // 4 * 4:02}'
        assert schedule_batch(f'2026-11-06T{hour:02}:59:59Z') == f'20261106{hour // 4 * 4:02}'
    assert schedule_batch('2026-11-07T00:23:00+08:00') == '2026110616'
    assert schedule_batch('2026-11-07T00:00:00Z') == '2026110700'
    for batch in ('20261106', '2026110601', '2026110624', '2026130600'):
        rejected(lambda: DailyPlan(batch, '', {}, 1, '3'))

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
                    value = {'expo': {'version': '0.0.1', 'ios': {'buildNumber': '3', 'supportsTablet': True},
                        'android': {'package': 'one.leaper.vibeface', 'versionCode': 250001},
                        'extra': {'releaseVersion': '0.0.1-nightly.20261105.2'}}}
                (directory / ('app.json' if app == 'mobile' else 'package.json')).write_text(json.dumps(value, indent=2) + '\n')
        (root / 'Cargo.toml').write_text('[workspace]\nmembers = ["apps/core", "apps/cli"]\nresolver = "2"\n')
        (root / 'Cargo.lock').write_text('version = 3\n\n[[package]]\nname = "vibeface"\nversion = "0.0.1-nightly.20261105.2"\n\n[[package]]\nname = "vibeface-core"\nversion = "0.0.1-nightly.20261105.2"\n')
        git('init', '--initial-branch=main', root=root)
        git('add', '.', root=root)
        git('-c', 'user.name=github-actions[bot]', '-c', 'user.email=' + BOT, 'commit', '-qm', 'fixture', root=root)
        real_check_output = subprocess.check_output

        def metadata_cargo_stub(cmd, cwd=None, **kwargs):
            if isinstance(cmd, (list, tuple)) and len(cmd) >= 2 and cmd[0] == 'cargo' and cmd[1] == 'metadata':
                work = Path(cwd)
                cli = re.search(r'version = "([^"]+)"', (work / 'apps/cli/Cargo.toml').read_text())[1]
                core = re.search(r'version = "([^"]+)"', (work / 'apps/core/Cargo.toml').read_text())[1]
                return json.dumps({'packages': [{'name': 'vibeface', 'version': cli},
                                                {'name': 'vibeface-core', 'version': core}]}).encode()
            return real_check_output(cmd, cwd=cwd, **kwargs)

        with patch(f'{__name__}.ROOT', root), patch('release.ROOT', root), patch(f'{__name__}.time.sleep'), \
                patch('subprocess.check_output', side_effect=metadata_cargo_stub):
            assert RELEASES == ('product', 'www') and PRODUCT_APPS == ('app', 'core', 'cli', 'mobile', 'desktop')
            base = git('rev-parse', 'HEAD')
            # Legacy component tags no longer claim a sequence; only real v/www-v tags and the current cohort do.
            plan = plan_versions('2026110600', base, ['v0.0.1-nightly.20261106.7', 'core-v0.0.1-nightly.20261106.9',
                                                    'mobile-v0.0.1-nightly.20261106.9', 'cli-v0.0.1-nightly.20261106.9'])
            assert plan.versions['core'] == '0.0.1-nightly.20261106.8'
            assert all(plan.versions[app] == plan.versions['cli'] for app in PRODUCT_APPS)
            assert plan.versions['www'] == '0.0.1-nightly.20261106.1'
            assert plan.ios_build_number == '4'
            assert next_ios_build_number('3.1.9') == '3.1.10'
            assert next_ios_build_number('3.01') == '3.2'
            for invalid in ('', '0', '10000', '9999', '3.99', '3.1.99', '3.100', '3.1.100', '3.1.2.3'):
                rejected(lambda invalid=invalid: next_ios_build_number(invalid))
            assert plan.day == '20261106' and plan.branch == 'bot/daily-nightly/2026110600'
            assert plan_versions('2026110500', base, []).versions['core'].endswith('.3')
            assert plan_versions('2026110600', base, ['v0.0.1-nightly.20261106.9']).versions['cli'].endswith('.10')
            assert plan_versions('2026110600', base, ['www-v0.0.1-nightly.20261106.4']).versions['www'].endswith('.5')
            assert plan_versions('2026110600', base, ['www-v0.0.1-nightly.20261106.4']).versions['cli'].endswith('.1')
            contents = generated(plan)
            mobile_generated = json.loads(contents['apps/mobile/app.json'])['expo']
            assert mobile_generated['ios']['buildNumber'] == '4'
            assert mobile_generated['ios']['supportsTablet'] is True
            assert json.loads(contents['apps/desktop/package.json'])['version'] == plan.versions['cli']
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
            assert recovered_plan(plan.batch, head) == plan
            verify_tree(plan, head)
            metadata(root, plan)
            for path, content in (
                ('apps/app/package.json', contents['apps/app/package.json'].replace(b'"private": true', b'"private": false')),
                ('apps/desktop/package.json', contents['apps/desktop/package.json'].replace(b'"private": true', b'"private": false')),
                ('Cargo.lock', contents['Cargo.lock'] + b'\n[[package]]\nname = "unexpected"\nversion = "1.0.0"\n'),
                ('apps/core/Cargo.toml', contents['apps/core/Cargo.toml'] + b'\n[dependencies]\nserde = "1"\n'),
                ('apps/cli/Cargo.toml', contents['apps/cli/Cargo.toml'].replace(b'0.0.1-nightly.20261106.8', b'0.0.1-nightly.20261106.9')),
                ('apps/mobile/app.json', contents['apps/mobile/app.json'].replace(b'250002', b'1')),
                ('apps/mobile/app.json', contents['apps/mobile/app.json'].replace(b'"buildNumber": "4"', b'"buildNumber": "3"')),
                ('apps/mobile/app.json', contents['apps/mobile/app.json'].replace(b'"buildNumber": "4"', b'"buildNumber": "5"')),
                ('apps/mobile/app.json', contents['apps/mobile/app.json'].replace(b'"supportsTablet": true', b'"supportsTablet": false')),
                ('extra.txt', b'not a version file\n'),
            ):
                bad = commit_files({path: content})
                rejected(lambda: verify_tree(plan, bad), 'mechanical')
                git('checkout', '--detach', '-q', base)
            # An old per-app operation is refused outright instead of being reinterpreted as a new cohort.
            stale = commit_files({'apps/mobile/app.json': contents['apps/mobile/app.json'].replace(
                b'"releaseVersion": "0.0.1-nightly.20261106.8"', b'"releaseVersion": "0.0.1-nightly.20261106.9"')})
            rejected(lambda: verify_tree(plan, stale), 'mechanical')
            rejected(lambda: verify_tree(recovered_plan(plan.batch, stale), stale), 'canonical version')
            git('checkout', '--detach', '-q', base)
            head = commit_files({})
            git('checkout', '--detach', '-q', base)
            (root / 'business.txt').write_text('main advanced independently\n')
            git('add', 'business.txt')
            git('-c', 'user.name=github-actions[bot]', '-c', 'user.email=' + BOT, 'commit', '-qm', 'independent main change')
            advanced = git('rev-parse', 'HEAD')
            landed = DailyPlan(plan.batch, advanced, plan.versions, plan.android_code, plan.ios_build_number)
            merged = git('-c', 'user.name=github-actions[bot]', '-c', 'user.email=' + BOT,
                         'commit-tree', expected_tree(landed), '-p', advanced, '-m', 'actual squash fixture')
            assert merged != head
            assert blob(merged, 'business.txt') == b'main advanced independently\n'
            git('update-ref', 'refs/remotes/origin/main', advanced)
            pr = dict(number=17, user=dict(login='nightly-owner'), head=dict(repo=dict(full_name=REPO), ref=plan.branch, sha=head),
                      base=dict(repo=dict(full_name=REPO), ref='main'), commits=1, draft=False,
                      title='chore(release): nightly ' + plan.batch, state='open', merged=False, mergeable=True)
            state = {'pr': None, 'merge_checks': 0}; writes = []
            real_git = git

            def fixture_git(*args, **kwargs):
                return '' if args[0] == 'fetch' else real_git(*args, **kwargs)

            def pr_api(path, method='GET', data=None, pat=False, missing=False):
                if method == 'POST':
                    assert path == 'pulls' and pat
                    assert data['head'] == plan.branch and data['base'] == 'main' and data['title'] == pr['title']
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
                with patch(f'{__name__}.branch_sha', return_value=head), patch(f'{__name__}.plan_versions', side_effect=AssertionError('same-batch reallocation')):
                    assert ensure_landed(plan.batch, 'nightly-owner') == (plan, merged, 17)
                rejected(lambda: validate_pr(dict(pr, user=dict(login='outsider')), plan, head, 'nightly-owner'), 'identity')
            next_batch = plan_versions('2026110604', merged, [])
            assert next_batch.day == plan.day and next_batch.branch != plan.branch
            assert next_batch.versions['cli'] == '0.0.1-nightly.20261106.9'
            assert next_batch.versions['www'] == '0.0.1-nightly.20261106.2'
            assert next_batch.android_code == plan.android_code + 1
            assert next_batch.ios_build_number == '5'
            new_remote = {}
            def next_batch_git(*args, **kwargs):
                if args[0] in ('fetch', 'ls-remote'):
                    return ''
                if args[0] == 'push':
                    pushed, ref = args[2].split(':')
                    assert ref == 'refs/heads/' + next_batch.branch
                    new_remote['head'] = pushed
                    return ''
                return real_git(*args, **kwargs)
            with patch(f'{__name__}.git', side_effect=next_batch_git), \
                    patch(f'{__name__}.daily_pr', side_effect=lambda branch: state['pr'] if branch == plan.branch else None) as lookup, \
                    patch(f'{__name__}.branch_sha', side_effect=lambda branch: new_remote.get('head')), \
                    patch(f'{__name__}.create_pr', return_value=dict(number=18)) as new_pr, \
                    patch(f'{__name__}.merge_pr', side_effect=lambda pr, plan, head, login: head):
                allocated, next_head, number = ensure_landed(next_batch.batch, 'nightly-owner')
                assert allocated == next_batch and number == 18
                lookup.assert_called_once_with(next_batch.branch)
                new_pr.assert_called_once_with(next_batch, next_head, 'nightly-owner')
            assert recovered_plan(next_batch.batch, next_head) == next_batch
            verify_tree(next_batch, next_head)
            next_day = plan_versions('2026110700', next_head, [])
            pv = next_day.versions['cli']
            assert pv == '0.0.1-nightly.20261107.1' and all(next_day.versions[a] == pv for a in PRODUCT_APPS)
            assert next_day.versions['www'] == '0.0.1-nightly.20261107.1'
            assert next_day.android_code == next_batch.android_code + 1
            assert next_day.ios_build_number == '6'
            future_main = git('-c', 'user.name=github-actions[bot]', '-c', 'user.email=' + BOT,
                              'commit-tree', expected_tree(next_day), '-p', next_head, '-m', 'next-day main fixture')
            git('update-ref', 'refs/remotes/origin/main', future_main)
            with patch(f'{__name__}.git', side_effect=next_batch_git), patch(f'{__name__}.daily_pr', return_value=None), \
                    patch(f'{__name__}.branch_sha', return_value=None), patch(f'{__name__}.api') as no_write, \
                    patch(f'{__name__}.checkout', side_effect=AssertionError('stale batch generated a branch')) as no_checkout:
                rejected(lambda: ensure_landed('2026110620', 'nightly-owner'), 'current cli version')
                no_checkout.assert_not_called()
                no_write.assert_not_called()
            newer_www = versions_at(next_head)
            newer_www['www'] = next_day.versions['www']
            with patch(f'{__name__}.versions_at', return_value=newer_www):
                rejected(lambda: plan_versions('2026110620', next_head, []), 'current www version')
            with patch(f'{__name__}.git', side_effect=fixture_git), patch(f'{__name__}.daily_pr', return_value=state['pr']), \
                    patch(f'{__name__}.branch_sha', return_value=head), \
                    patch(f'{__name__}.plan_versions', side_effect=AssertionError('old batch was reallocated')):
                assert ensure_landed(plan.batch, 'nightly-owner') == (plan, merged, 17)
            git('update-ref', 'refs/remotes/origin/main', merged)

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
                    if path == 'git/refs':
                        remote['tag'] = data['sha']
                    elif path.endswith('/dispatches'):
                        # A real daily release must request publication, not a build-only dry run.
                        assert data == dict(ref=tag, inputs=dict(app='product', tag=tag, publish='true'))
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
            remote_state = {
                'artifacts': [dict(produced, id=51, name=f'product-release-{merged}-1'),
                              dict(produced, id=52, name=f'npm-package-{merged}-1')],
                'jobs': [dict(name='collect', status='completed', conclusion='success'),
                         dict(name='publish', status='completed', conclusion='success'),
                         dict(name='npm / npm', status='completed', conclusion='failure')],
            }

            def recovery_pages(path, key):
                return list(remote_state['artifacts' if key == 'artifacts' else 'jobs'])

            failing = dict(good, conclusion='failure')
            with patch(f'{__name__}.api', side_effect=recovery_api), patch(f'{__name__}.pages', side_effect=recovery_pages):
                # A public product Release whose npm job failed is recovered from the original immutable artifacts.
                assert original_artifacts('product', failing, merged) == {
                    f'product-release-{merged}-': 51, f'npm-package-{merged}-': 52}
                assert recover_publisher('product', failing, merged)['run_attempt'] == 2
                assert writes == ['actions/runs/1/rerun-failed-jobs']
                for artifacts in ([], [dict(produced, id=51, name=f'product-release-{merged}-1')],
                                  [dict(produced, id=51, name=f'product-release-{merged}-1'),
                                   dict(produced, id=52, name=f'npm-package-{merged}-2')],
                                  [dict(produced, id=51, name=f'product-release-{merged}-1'),
                                   dict(produced, id=52, name=f'npm-package-{merged}-1', expired=True)],
                                  [dict(produced, id=51, name=f'product-release-{merged}-1'),
                                   dict(produced, id=52, name=f'npm-package-{merged}-1', workflow_run=dict(id=9, head_sha=merged))],
                                  [dict(produced, id=51, name=f'product-release-{merged}-1'),
                                   dict(produced, id=52, name=f'npm-package-{merged}-1', workflow_run=dict(id=1, head_sha='b' * 40))]):
                    with patch.dict(remote_state, {'artifacts': artifacts}):
                        rejected(lambda: recover_publisher('product', failing, merged), 'refusing a rebuild')
                for jobs in ([dict(name='collect', status='completed', conclusion='success'),
                              dict(name='compose-macos', status='completed', conclusion='failure')],
                             [dict(name='collect', status='completed', conclusion='success'),
                              dict(name='npm / npm', status='in_progress', conclusion=None)],
                             [dict(name='collect', status='completed', conclusion='success'),
                              dict(name='publish', status='completed', conclusion='success')]):
                    with patch.dict(remote_state, {'jobs': jobs}):
                        rejected(lambda: recover_publisher('product', failing, merged), 'only an original-artifact publisher')
                for jobs in ([dict(name='collect', status='completed', conclusion='failure'),
                              dict(name='publish', status='completed', conclusion='failure')],
                             [dict(name='publish', status='completed', conclusion='failure')]):
                    with patch.dict(remote_state, {'jobs': jobs}):
                        rejected(lambda: recover_publisher('product', failing, merged), 'producer did not succeed')
                rejected(lambda: recover_publisher('mobile', failing, merged), 'product and www')
                assert len(writes) == 1

            with patch(f'{__name__}.tag_sha', return_value=merged), patch(f'{__name__}.release_state', return_value=dict(draft=False)), \
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

            with patch(f'{__name__}.tag_sha', return_value=merged), patch(f'{__name__}.release_state', return_value=None), \
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
                               GITHUB_RUN_ID='42', GITHUB_SHA=merged, NIGHTLY_RELEASE_TOKEN='fixture-only', GH_TOKEN='fixture-only')
            original = dict(good, event='schedule', created_at='2026-11-06T00:23:00Z')
            observed_batches = []
            def observe_batch(batch, login):
                observed_batches.append(batch)
                raise ValueError('batch recovered from original run')
            for now, attempt in ((datetime.datetime(2026, 11, 6, 23, tzinfo=datetime.timezone.utc), 1),
                                 (datetime.datetime(2026, 11, 7, 12, tzinfo=datetime.timezone.utc), 2)):
                original['run_attempt'] = attempt
                with patch.dict(os.environ, environment), \
                        patch(f'{__name__}.api', side_effect=lambda path, **kwargs: dict(login='owner') if path == '/user' else original), \
                        patch(f'{__name__}.ensure_landed', side_effect=observe_batch), \
                        patch(f'{__name__}.datetime.datetime', wraps=datetime.datetime) as clock:
                    clock.now.return_value = now
                    rejected(lambda: run_daily(42), 'batch recovered')
                    clock.now.assert_not_called()
            assert observed_batches == ['2026110600', '2026110600']
            for changes in ({'NIGHTLY_RELEASE_TOKEN': ''}, {'GITHUB_EVENT_NAME': 'workflow_dispatch'}):
                with patch.dict(os.environ, dict(environment, **changes)), patch(f'{__name__}.api') as no_write:
                    rejected(lambda: run_daily(42))
                    no_write.assert_not_called()

    workflow = (ROOT / CI).read_text()
    assert "cron: '23 */4 * * *'" in workflow and 'group: daily-nightly\n      cancel-in-progress: false' in workflow
    assert "if: ${{ !cancelled() && github.event_name == 'schedule' && github.ref == 'refs/heads/main' && needs.ci-gate.result == 'success' }}" in workflow
    npm = (ROOT / '.github/workflows/npm-publish.yml').read_text()
    assert 'id-token: write' in npm and 'environment: npm-release' in npm and 'default: false' in npm
    assert 'NPM_TOKEN' not in npm and 'secrets.' not in npm
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
