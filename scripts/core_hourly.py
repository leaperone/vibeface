#!/usr/bin/env python3
"""Tag and dispatch one independent Core Hourly release per UTC hour."""
import argparse
import datetime
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time

import core_release

REPO = core_release.REPOSITORY
BOT = core_release.BOT_EMAIL
TAG_PATTERN = re.compile(r'core-v[0-9]+\.[0-9]+\.[0-9]+-hourly\.([0-9]{10})')
REQUIRED_PATHS = ('.github/workflows/core-hourly.yml', '.github/workflows/release-core.yml',
                  'scripts/core_hourly.py', 'scripts/core_release.py')


def git(*args, root=None, env=None):
    return subprocess.check_output(['git', *args], cwd=root or core_release.ROOT, env=env).decode().strip()


def api(path, method='GET', data=None, missing=False):
    args = ['gh', 'api', '--method', method, f'repos/{REPO}/{path}']
    if data is not None:
        args += ['--input', '-']
    result = subprocess.run(args, input=json.dumps(data) if data is not None else None,
                            capture_output=True, text=True)
    if missing and result.returncode and '(HTTP 404)' in result.stderr:
        return None
    core_release.require(result.returncode == 0, f'GitHub {method} {path} failed')
    return json.loads(result.stdout) if result.stdout.strip() else None


def green_main():
    runs = api('actions/workflows/ci.yml/runs?branch=main&event=push&status=success&per_page=100')['workflow_runs']
    green = {run['head_sha'] for run in runs if run['head_branch'] == 'main' and run['path'] == '.github/workflows/ci.yml'
             and run['repository']['full_name'] == REPO and run['head_repository']['full_name'] == REPO}
    for commit in git('rev-list', '--first-parent', '-n', '200', 'origin/main').splitlines():
        paths = set(git('ls-tree', '-r', '--name-only', commit, '--', *REQUIRED_PATHS).splitlines())
        if commit in green and set(REQUIRED_PATHS) <= paths:
            return commit
    raise ValueError('no main commit with successful push CI and the Core train workflow among the latest 200')


def tag_commit(tag):
    ref = api('git/ref/tags/' + tag, missing=True)
    if not ref:
        return None
    item = ref['object']
    for _ in range(8):
        if item['type'] == 'commit':
            return item['sha']
        core_release.require(item['type'] == 'tag', 'Core tag is not a commit or annotated tag')
        item = api('git/tags/' + item['sha'])['object']
    raise ValueError('Core tag nesting is too deep')


def release_runs(tag, source):
    runs = api(f'actions/workflows/release-core.yml/runs?branch={tag}&head_sha={source}&per_page=100')['workflow_runs']
    return [run for run in runs if run['head_sha'] == source and run['head_branch'] == tag
            and run['repository']['full_name'] == REPO]


def confirm(write, read, description):
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


def run_hourly(now):
    core_release.require(os.environ.get('GH_TOKEN') and os.environ.get('GITHUB_REPOSITORY') == REPO
                         and os.environ.get('GITHUB_REF') == 'refs/heads/main',
                         'Core Hourly coordinator runs only from main in Actions')
    batch = now.astimezone(datetime.timezone.utc).strftime('%Y%m%d%H')
    git('fetch', '--quiet', 'origin', 'main', '--tags')
    base = green_main()
    tags = [line.split('\t')[1][len('refs/tags/'):] for line in git('ls-remote', '--tags', 'origin', 'core-v*-hourly.*').splitlines()
            if not line.endswith('^{}')]
    hourly = sorted((match[1], tag) for tag in tags if (match := TAG_PATTERN.fullmatch(tag)))
    version = core_release.core_hourly_version(base, batch)
    tag = 'core-v' + version
    source = tag_commit(tag)
    if source is None:
        identity = dict(os.environ, GIT_AUTHOR_NAME='github-actions[bot]', GIT_AUTHOR_EMAIL=BOT,
                        GIT_COMMITTER_NAME='github-actions[bot]', GIT_COMMITTER_EMAIL=BOT)
        _, tree = core_release.generated_tree(base, batch)
        source = git('commit-tree', tree, '-p', base, '-m', f'chore(release): core hourly {batch}', env=identity)
        core_release.require(core_release.verify_hourly(source) == (base, batch), 'Core Hourly source failed its gate')
        confirm(lambda: git('push', 'origin', f'{source}:refs/tags/{tag}'), lambda: tag_commit(tag), 'Core Hourly tag creation')
        core_release.require(tag_commit(tag) == source, 'Core Hourly tag points to another commit')
    else:
        core_release.verify_hourly(source)
    runs = release_runs(tag, source)
    if not runs:
        runs = confirm(lambda: api('actions/workflows/release-core.yml/dispatches', 'POST',
                                   dict(ref=tag, inputs=dict(tag=tag, publish='true'))),
                       lambda: release_runs(tag, source), 'Core Hourly release dispatch')
    print(json.dumps(dict(batch=batch, tag=tag, source_commit=source, main_commit=base,
                          release_runs=[run['html_url'] for run in runs]), indent=2))


def self_test():
    assert REQUIRED_PATHS == ('.github/workflows/core-hourly.yml', '.github/workflows/release-core.yml',
                              'scripts/core_hourly.py', 'scripts/core_release.py')
    core_release.self_test()
    print('Core Hourly self-test passed')


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
