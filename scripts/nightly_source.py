"""Exact mechanical Nightly trees shared by every release source gate."""
from contextlib import contextmanager
from dataclasses import dataclass
import datetime
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile

import release

PATHS = ('apps/www/package.json', 'apps/app/package.json', 'apps/core/Cargo.toml',
         'apps/cli/Cargo.toml', 'Cargo.lock', 'apps/mobile/app.json', 'apps/desktop/package.json')
PRODUCT_APPS = release.PRODUCT_APPS
BOT = '41898282+github-actions[bot]@users.noreply.github.com'
require = release.require


@dataclass(frozen=True)
class DailyPlan:
    batch: str
    base: str
    versions: dict
    android_code: int
    ios_build_number: str
    run_id: str
    previous: str = ''

    def __post_init__(self):
        slot = datetime.datetime.strptime(self.batch, '%Y%m%d%H')
        require(slot.strftime('%Y%m%d%H') == self.batch and slot.hour % 4 == 0,
                'Nightly batch must be a four-hour UTC slot')
        require(re.fullmatch(r'[1-9][0-9]*', self.run_id), 'Nightly source must name its original schedule run')
        require(not self.previous or re.fullmatch(release.COMMIT, self.previous), 'invalid previous Nightly source')

    @property
    def day(self):
        return self.batch[:8]

    @property
    def message(self):
        return (f'chore(release): nightly {self.batch}\n\nNightly-CI-Run: {self.run_id}\n'
                f'Nightly-Previous: {self.previous or "none"}')


def git(*args, root=None, data=None, env=None):
    return subprocess.check_output(['git', *args], cwd=root or release.ROOT, input=data, env=env).decode().strip()


def blob(commit, path):
    return subprocess.check_output(['git', 'show', f'{commit}:{path}'], cwd=release.ROOT)


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


def next_ios_build_number(value):
    """Increment the last valid CFBundleVersion component without rollover."""
    ios_number(value)
    parts = value.split('.')
    maximum = 9999 if len(parts) == 1 else 99
    last = int(parts[-1])
    require(last < maximum, 'iOS build number component cannot be incremented without rollover')
    parts[-1] = str(last + 1)
    return '.'.join(parts)


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
        if release.version(old[app]) == 'nightly':
            require(nightly_order(value) > nightly_order(old[app]), 'daily version did not advance')
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
    code, ios = next_build_numbers(plan.base, plan.previous)
    require(plan.android_code == code, 'Android versionCode must advance exactly once')
    require(plan.ios_build_number == ios, 'iOS build number must advance exactly once')
    result[mobile_path] = json_value(result[mobile_path], ('expo', 'extra', 'releaseVersion'), plan.versions['mobile'])
    result[mobile_path] = json_value(result[mobile_path], ('expo', 'android', 'versionCode'), plan.android_code)
    result[mobile_path] = json_value(result[mobile_path], ('expo', 'ios', 'buildNumber'), plan.ios_build_number)
    return {path: text.encode() for path, text in result.items()}


def recovered_plan(head):
    parents = git('rev-list', '--parents', '-n', '1', head).split()
    require(len(parents) == 2, 'daily version head must be one ordinary commit')
    message = git('show', '-s', '--format=%B', head)
    match = re.fullmatch(r'chore\(release\): nightly ([0-9]{10})\n\nNightly-CI-Run: ([1-9][0-9]*)\nNightly-Previous: (none|[a-f0-9]{40})', message)
    require(match, 'Nightly source is missing its original schedule identity')
    expo = json.loads(blob(head, 'apps/mobile/app.json'))['expo']
    return DailyPlan(match[1], parents[1], versions_at(head), expo['android']['versionCode'],
                     expo['ios']['buildNumber'], match[2], '' if match[3] == 'none' else match[3])


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
            'Nightly tree differs from exact mechanical version changes')


def scratch():
    directory = Path(os.environ.get('VIBEFACE_RELEASE_SCRATCH', release.ROOT / '.artifacts/nightly-source'))
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


def nightly_order(value):
    require(release.version(value) == 'nightly', 'previous product source must be Nightly')
    baseline, suffix = value.split('-nightly.')
    day, sequence = suffix.split('.')
    return (*map(int, baseline.split('.')), int(day), int(sequence))


def ios_number(value):
    require(isinstance(value, str), 'iOS build number must be a string')
    parts = value.split('.')
    require(1 <= len(parts) <= 3 and re.fullmatch(r'[0-9]{1,4}', parts[0])
            and int(parts[0]) > 0 and all(re.fullmatch(r'[0-9]{1,2}', part) for part in parts[1:]),
            'iOS build number must be a valid Apple CFBundleVersion')
    numeric = tuple(map(int, parts))
    return numeric + (0,) * (3 - len(numeric))


def next_build_numbers(base, previous=''):
    versions = [json.loads(blob(commit, 'apps/mobile/app.json'))['expo']
                for commit in (base, previous) if commit]
    codes = [expo['android']['versionCode'] for expo in versions]
    require(all(type(code) is int and 0 < code < 2100000000 for code in codes),
            'Android versionCode cannot be incremented')
    ios = max((expo['ios']['buildNumber'] for expo in versions), key=ios_number)
    return max(codes) + 1, next_ios_build_number(ios)


_verified_sources = {}


def source_context(source, release_app):
    require(release_app in release.RELEASES and re.fullmatch(release.COMMIT, source), 'invalid release source')
    context = dict(ci_source_commit=source, ci_run_id='', nightly_batch='')
    ancestry = subprocess.run(['git', 'merge-base', '--is-ancestor', source, 'origin/main'],
                              cwd=release.ROOT, capture_output=True)
    if ancestry.returncode == 0:
        return context
    require(ancestry.returncode == 1, 'cannot verify release main ancestry')
    origin = source
    repository = str((release.ROOT / git('rev-parse', '--git-common-dir')).resolve())
    main = git('rev-parse', 'origin/main')
    pending = []
    seen = set()
    while True:
        key = (repository, main, source, release_app)
        if key in _verified_sources:
            if source == origin:
                context = _verified_sources[key]
            break
        require(source not in seen, 'cyclic previous Nightly source')
        seen.add(source)
        ancestry = subprocess.run(['git', 'merge-base', '--is-ancestor', source, 'origin/main'],
                                  cwd=release.ROOT, capture_output=True)
        if ancestry.returncode == 0:
            pending.append((key, dict(ci_source_commit=source, ci_run_id='', nightly_batch='')))
            break
        require(ancestry.returncode == 1, 'cannot verify release main ancestry')
        values = versions_at(source)
        require(release.version(values['cli' if release_app == 'product' else 'www']) == 'nightly',
                'only Nightly mechanical sources may be outside main')
        plan = recovered_plan(source)
        require(subprocess.run(['git', 'merge-base', '--is-ancestor', plan.base, 'origin/main'],
                               cwd=release.ROOT, capture_output=True).returncode == 0,
                'Nightly parent must be on main')
        if plan.previous:
            require(nightly_order(versions_at(plan.previous)['cli']) < nightly_order(values['cli']),
                    'previous Nightly must precede this product version')
        verify_tree(plan, source)
        with checkout(source) as clean:
            metadata(clean, plan)
        current = dict(ci_source_commit=plan.base, ci_run_id=plan.run_id, nightly_batch=plan.batch)
        pending.append((key, current))
        if source == origin:
            context = current
        if not plan.previous:
            break
        source = plan.previous
        release_app = 'product'
    _verified_sources.update(pending)
    return context
