#!/usr/bin/env python3
"""Tag the newest green main commit once per UTC hour as a product Hourly release."""
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

sys.dont_write_bytecode = True
import release

REPO = release.GITHUB_REPO
PATHS = ('apps/app/package.json', 'apps/core/Cargo.toml', 'apps/cli/Cargo.toml', 'apps/desktop/Cargo.toml',
         'apps/mobile/app.json', 'Cargo.lock')
RUST = {'core': 'vibeface-core', 'cli': 'vibeface', 'desktop': 'vibeface-desktop'}
BOT = '41898282+github-actions[bot]@users.noreply.github.com'
HOURLY_TAG = re.compile(r'v[0-9]+\.[0-9]+\.[0-9]+-hourly\.([0-9]{10})')
require = release.require


def git(*args, root=None, data=None, env=None):
    return subprocess.check_output(['git', *args], cwd=root or release.ROOT, input=data, env=env).decode().strip()


def blob(commit, path):
    return subprocess.check_output(['git', 'show', f'{commit}:{path}'], cwd=release.ROOT).decode()


def hour_of(batch):
    require(isinstance(batch, str) and re.fullmatch(r'[0-9]{10}', batch), 'Hourly batch must be YYYYMMDDHH')
    datetime.datetime.strptime(batch, '%Y%m%d%H')
    return batch


def build_numbers(batch):
    """Android versionCode YYMMDDHH and iOS CFBundleVersion YYMM.DD.HH both rise with every hour."""
    hour_of(batch)
    return int(batch[2:]), f'{int(batch[2:6])}.{int(batch[6:8])}.{int(batch[8:])}'


def hourly_version(base, batch):
    value = blob(base, 'apps/cli/Cargo.toml')
    current = re.search(r'(?m)^version\s*=\s*"([^"]+)"', value)[1]
    result = current.split('-', 1)[0] + '-hourly.' + hour_of(batch)
    require(release.version(result) == 'hourly', 'invalid Hourly version')
    return result


def json_value(text, keys, value):
    data = json.loads(text)
    require(json.dumps(data, indent=2, ensure_ascii=False) + '\n' == text, 'version JSON formatting changed')
    parent = data
    for key in keys[:-1]:
        parent = parent[key]
    require(keys[-1] in parent, 'missing version field')
    parent[keys[-1]] = value
    return json.dumps(data, indent=2, ensure_ascii=False) + '\n'


def cargo_version(text, value):
    result, count = re.subn(r'(?ms)(^\[package\]\s*\n(?:(?!^\[).)*?^version\s*=\s*)"[^"]+"',
                            lambda match: match[1] + json.dumps(value), text, count=1)
    require(count == 1, 'missing Cargo package version')
    return result


def generated(base, batch):
    """The only bytes an Hourly source may change relative to its green main parent."""
    value = hourly_version(base, batch)
    code, ios = build_numbers(batch)
    result = {path: blob(base, path) for path in PATHS}
    result['apps/app/package.json'] = json_value(result['apps/app/package.json'], ('version',), value)
    for app in RUST:
        path = f'apps/{app}/Cargo.toml'
        result[path] = cargo_version(result[path], value)
    blocks = result['Cargo.lock'].split('[[package]]')
    found = []
    for index, block in enumerate(blocks):
        name = re.search(r'(?m)^name = "([^"]+)"$', block)
        if name and name[1] in RUST.values():
            require(not re.search(r'(?m)^(source|checksum)\s*=', block), 'Cargo.lock local package has a registry source')
            blocks[index] = re.sub(r'(?m)^version = "[^"]+"$', 'version = ' + json.dumps(value), block, count=1)
            found.append(name[1])
    require(sorted(found) == sorted(RUST.values()), 'Cargo.lock must hold each local package once')
    result['Cargo.lock'] = '[[package]]'.join(blocks)
    mobile = 'apps/mobile/app.json'
    for keys, item in ((('expo', 'extra', 'releaseVersion'), value), (('expo', 'android', 'versionCode'), code),
                       (('expo', 'ios', 'buildNumber'), ios)):
        result[mobile] = json_value(result[mobile], keys, item)
    return value, {path: text.encode() for path, text in result.items()}


def expected_tree(base, batch):
    _, files = generated(base, batch)
    with tempfile.TemporaryDirectory() as temporary:
        environment = dict(os.environ, GIT_INDEX_FILE=str(Path(temporary) / 'index'))
        git('read-tree', base, env=environment)
        for path, content in files.items():
            require(git('ls-tree', base, '--', path).split()[:2] == ['100644', 'blob'], 'version source must be a tracked file')
            oid = git('hash-object', '-w', '--stdin', data=content)
            git('update-index', '--add', '--cacheinfo', '100644', oid, path, env=environment)
        return git('write-tree', env=environment)


def message(batch):
    return f'chore(release): hourly {batch}'


def verify(source):
    """Return (base, batch) only for the exact bot commit that changes nothing but Hourly version fields."""
    commits = git('rev-list', '--parents', '-n', '1', source).split()
    require(len(commits) == 2 and commits[0] == source, 'Hourly source must be one ordinary commit')
    base = commits[1]
    match = re.fullmatch(r'chore\(release\): hourly ([0-9]{10})', git('show', '-s', '--format=%B', source))
    require(match, 'Hourly source is missing its batch')
    require(git('show', '-s', '--format=%ae%n%ce', source).splitlines() == [BOT, BOT], 'unexpected Hourly commit author')
    require(expected_tree(base, match[1]) == git('rev-parse', source + '^{tree}'),
            'Hourly tree differs from the exact mechanical version changes')
    return base, match[1]


def source_context(source, release_app):
    """Main sources prove themselves; a detached source must be the Hourly commit on a main parent."""
    require(release_app in release.RELEASES and re.fullmatch(release.COMMIT, source), 'invalid release source')
    if subprocess.run(['git', 'merge-base', '--is-ancestor', source, 'origin/main'], cwd=release.ROOT).returncode == 0:
        return dict(ci_source_commit=source)
    require(release_app == 'product', 'only product Hourly sources may be outside main')
    base, _ = verify(source)
    require(subprocess.run(['git', 'merge-base', '--is-ancestor', base, 'origin/main'], cwd=release.ROOT).returncode == 0,
            'Hourly parent must be on main')
    return dict(ci_source_commit=base)


def scratch():
    directory = release.ROOT / '.artifacts' / 'hourly'
    directory.mkdir(parents=True, exist_ok=True)
    return directory


def api(path, method='GET', data=None, missing=False):
    args = ['gh', 'api', '--method', method, f'repos/{REPO}/{path}'] + (['--input', '-'] if data is not None else [])
    result = subprocess.run(args, input=json.dumps(data) if data is not None else None, capture_output=True, text=True)
    if missing and result.returncode and '(HTTP 404)' in result.stderr:
        return None
    require(result.returncode == 0, f'GitHub {method} {path} failed')
    return json.loads(result.stdout) if result.stdout.strip() else None


def confirm(write, read, description):
    # A failed response can follow a successful write; only the remote state decides, never a second write.
    try:
        write()
    except (ValueError, subprocess.SubprocessError):
        pass
    for _ in range(18):
        value = read()
        if value:
            return value
        time.sleep(10)
    raise ValueError(description + ' has an unknown outcome; reconcile before the next hour')


def green_main():
    """Newest first-parent main commit whose push Core CI succeeded."""
    runs = api('actions/workflows/ci.yml/runs?branch=main&event=push&status=success&per_page=100')['workflow_runs']
    green = {run['head_sha'] for run in runs if run['head_branch'] == 'main' and run['path'] == '.github/workflows/ci.yml'
             and run['repository']['full_name'] == REPO and run['head_repository']['full_name'] == REPO}
    for commit in git('rev-list', '--first-parent', '-n', '200', 'origin/main').splitlines():
        if commit in green:
            return commit
    raise ValueError('no main commit with successful push CI among the latest 200')


def tag_commit(tag):
    ref = api('git/ref/tags/' + tag, missing=True)
    if not ref:
        return None
    item = ref['object']
    for _ in range(8):
        if item['type'] == 'commit':
            return item['sha']
        require(item['type'] == 'tag', 'release ref is not a commit or annotated tag')
        item = api('git/tags/' + item['sha'])['object']
    raise ValueError('too many nested tag objects')


def release_runs(tag, source):
    runs = api(f'actions/workflows/release.yml/runs?branch={tag}&head_sha={source}&per_page=100')['workflow_runs']
    return [run for run in runs if run['head_sha'] == source and run['head_branch'] == tag
            and run['repository']['full_name'] == REPO]


def create_source(base, batch):
    identity = dict(os.environ, GIT_AUTHOR_NAME='github-actions[bot]', GIT_AUTHOR_EMAIL=BOT,
                    GIT_COMMITTER_NAME='github-actions[bot]', GIT_COMMITTER_EMAIL=BOT)
    source = git('commit-tree', expected_tree(base, batch), '-p', base, '-m', message(batch), env=identity)
    require(verify(source) == (base, batch), 'created Hourly source failed its own gate')
    return source


def release_settled(tag, source):
    """A previous hour counts only while its release is running or succeeded; failed hours are rebuilt."""
    return any(run['status'] != 'completed' or run['conclusion'] == 'success' for run in release_runs(tag, source))


def run_hourly(now):
    require(os.environ.get('GH_TOKEN') and os.environ.get('GITHUB_REPOSITORY') == REPO
            and os.environ.get('GITHUB_REF') == 'refs/heads/main', 'Hourly coordinator runs only from main in Actions')
    batch = now.astimezone(datetime.timezone.utc).strftime('%Y%m%d%H')
    git('fetch', '--quiet', 'origin', 'main', '--tags')
    base = green_main()
    tags = [line.split('\t')[1][len('refs/tags/'):] for line in git('ls-remote', '--tags', 'origin', 'v*-hourly.*').splitlines()
            if not line.endswith('^{}')]
    hourly = sorted((match[1], tag) for tag in tags if (match := HOURLY_TAG.fullmatch(tag)))
    tag = 'v' + hourly_version(base, batch)
    source = tag_commit(tag)
    if source is None:
        if hourly:
            require(hourly[-1][0] < batch, 'a later Hourly tag already exists; refusing to go back in time')
            previous = tag_commit(hourly[-1][1])
            parent = git('rev-parse', previous + '^')
            if subprocess.run(['git', 'merge-base', '--is-ancestor', parent, base], cwd=release.ROOT).returncode:
                print(json.dumps(dict(batch=batch, skipped='newest green main commit precedes the latest Hourly')))
                return
            if parent == base and release_settled(hourly[-1][1], previous):
                print(json.dumps(dict(batch=batch, skipped='no new green main commit', latest=hourly[-1][1])))
                return
        source = create_source(base, batch)
        with tempfile.TemporaryDirectory(dir=scratch()) as temporary:
            clean = Path(temporary) / 'plan'
            git('worktree', 'add', '--detach', str(clean), source)
            previous_root = release.ROOT
            release.ROOT = clean
            try:
                release.plan('product', tag=tag, require_main=True)
                release.require_ci(release.validate_source(source, 'product'))
            finally:
                release.ROOT = previous_root
                git('worktree', 'remove', '--force', str(clean))
        confirm(lambda: git('push', 'origin', f'{source}:refs/tags/{tag}'), lambda: tag_commit(tag), 'Hourly tag creation')
        require(tag_commit(tag) == source, 'Hourly tag points to another commit')
    else:
        verify(source)
    runs = release_runs(tag, source)
    if not runs:
        runs = confirm(lambda: api('actions/workflows/release.yml/dispatches', 'POST',
                                   dict(ref=tag, inputs=dict(tag=tag, publish='true'))),
                       lambda: release_runs(tag, source), 'Hourly release dispatch')
    print(json.dumps(dict(batch=batch, tag=tag, source_commit=source, main_commit=base,
                          release_runs=[run['html_url'] for run in runs]), indent=2))


def self_test():
    head = git('rev-parse', 'HEAD')
    batch = '2026111309'
    assert build_numbers(batch) == (26111309, '2611.13.9')
    assert build_numbers('2026123123')[0] > build_numbers('2026111309')[0]
    for bad in ('2026111324', '20261113', '2026021309x'):
        try:
            hour_of(bad)
        except ValueError:
            continue
        raise AssertionError(f'accepted invalid batch {bad}')
    assert release.version('0.0.1-hourly.2026111309') == 'hourly'
    for bad in ('0.0.1-nightly.20261113.1', '0.0.1-hourly.2026111324', '0.0.1-hourly.202611130'):
        try:
            release.version(bad)
        except ValueError:
            continue
        raise AssertionError(f'accepted invalid version {bad}')
    value, files = generated(head, batch)
    assert value.endswith('-hourly.' + batch) and set(files) == set(PATHS)
    mobile = json.loads(files['apps/mobile/app.json'])['expo']
    assert (mobile['extra']['releaseVersion'], mobile['android']['versionCode'], mobile['ios']['buildNumber']) == (
        value, 26111309, '2611.13.9')
    assert files['Cargo.lock'].decode().count(f'version = "{value}"') == 3
    tree = expected_tree(head, batch)
    bot = dict(os.environ, GIT_AUTHOR_NAME='bot', GIT_AUTHOR_EMAIL=BOT, GIT_COMMITTER_NAME='bot', GIT_COMMITTER_EMAIL=BOT,
               GIT_AUTHOR_DATE='2026-11-13T09:00:00Z', GIT_COMMITTER_DATE='2026-11-13T09:00:00Z')
    good = git('commit-tree', tree, '-p', head, '-m', message(batch), env=bot)
    assert verify(good) == (head, batch)
    tampered = git('commit-tree', git('rev-parse', head + '^{tree}'), '-p', head, '-m', message(batch), env=bot)
    human = git('commit-tree', tree, '-p', head, '-m', message(batch),
                env=dict(bot, GIT_AUTHOR_EMAIL='someone@example.com'))
    wrong_hour = git('commit-tree', tree, '-p', head, '-m', message('2026111310'), env=bot)
    for commit in (tampered, human, wrong_hour):
        try:
            verify(commit)
        except ValueError:
            continue
        raise AssertionError('accepted a non-mechanical Hourly source')
    print('Hourly version, build number and exact mechanical source self-tests passed')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--self-test', action='store_true')
    args = parser.parse_args()
    if args.self_test:
        self_test()
    else:
        run_hourly(datetime.datetime.now(datetime.timezone.utc))


if __name__ == '__main__':
    main()
