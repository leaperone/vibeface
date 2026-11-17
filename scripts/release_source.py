"""Exact Hourly generation trees and release-source eligibility proofs."""
import datetime
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import release_context
import release_format


BOT = '41898282+github-actions[bot]@users.noreply.github.com'


RUST = {'core': 'vibeface-core', 'cli': 'vibeface', 'desktop': 'vibeface-desktop'}


def hour_of(batch):
    release_format.require(isinstance(batch, str) and re.fullmatch(r'[0-9]{10}', batch), 'Hourly batch must be YYYYMMDDHH')
    datetime.datetime.strptime(batch, '%Y%m%d%H')
    return batch


def build_numbers(batch):
    hour_of(batch)
    return int(batch[2:]), f'{int(batch[2:6])}.{int(batch[6:8])}.{int(batch[8:])}'


def version_path(app):
    return f'apps/{app}/' + ('Cargo.toml' if app in RUST else 'app.json' if app == 'mobile' else 'package.json')


def hourly_version(app, base, batch, root=None):
    text = release_context.blob(base, version_path(app), root).decode()
    if app in RUST:
        current = re.search(r'(?m)^version\s*=\s*"([^"]+)"', text)[1]
    else:
        data = json.loads(text)
        current = data['expo']['extra']['releaseVersion'] if app == 'mobile' else data['version']
    result = current.split('-', 1)[0] + '-hourly.' + hour_of(batch)
    release_format.version(result)
    return result


def generated(app, base, batch, pins=None, root=None):
    release_format.require(app in release_format.APPS, 'unknown Hourly application')
    pins = pins or {}
    release_format.require(set(pins) == set(release_format.DEPENDENCIES[app]), 'Hourly dependency set differs')
    for dependency, pin in pins.items():
        release_format.validate_pin(dependency, pin)
    value = hourly_version(app, base, batch, root)
    path = version_path(app)
    text = release_context.blob(base, path, root).decode()
    result = {}
    if app in RUST:
        text, count = re.subn(r'(?m)^(version\s*=\s*)"[^"]+"', lambda m: m[1] + json.dumps(value), text, count=1)
        release_format.require(count == 1, 'missing application version')
        blocks = release_context.blob(base, 'Cargo.lock', root).split(b'[[package]]')
        count = 0
        for index, block in enumerate(blocks):
            if re.search(rb'(?m)^name = "' + RUST[app].encode() + rb'"$', block):
                release_format.require(not re.search(rb'(?m)^(source|checksum)\s*=', block), 'local package cannot be a registry package')
                blocks[index] = re.sub(rb'(?m)^version = "[^"]+"$', b'version = ' + json.dumps(value).encode(), block, count=1)
                count += 1
        release_format.require(count == 1, 'Cargo.lock must contain selected local package once')
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
        release_context.git('read-tree', base, env=env, root=root)
        for path, data in content.items():
            oid = release_context.git('hash-object', '-w', '--stdin', data=data, root=root)
            release_context.git('update-index', '--add', '--cacheinfo', '100644', oid, path, env=env, root=root)
        return release_context.git('write-tree', env=env, root=root)


def verify(source, app, root=None):
    parts = release_context.git('rev-list', '--parents', '-n', '1', source, root=root).split()
    release_format.require(len(parts) == 2 and parts[0] == source, 'Hourly source must be one ordinary commit')
    base = parts[1]
    match = re.fullmatch(r'chore\(release\): ' + app + r' hourly ([0-9]{10})', release_context.git('show', '-s', '--format=%B', source, root=root))
    release_format.require(match, 'Hourly application/message mismatch')
    release_format.require(release_context.git('show', '-s', '--format=%ae%n%ce', source, root=root).splitlines() == [BOT, BOT], 'unexpected Hourly author')
    pins = json.loads(release_context.blob(source, f'releases/{app}.json', root)) if release_format.DEPENDENCIES[app] else {}
    release_format.require(expected_tree(app, base, match[1], pins, root) == release_context.git('rev-parse', source + '^{tree}', root=root), 'Hourly source differs from exact selected version and pins')
    return base, match[1]


def validate_source(source, app, root=None):
    root = root or release_context.ROOT
    release_format.require(app in release_format.APPS and isinstance(source, str) and re.fullmatch(release_format.COMMIT, source), 'invalid release source')
    if subprocess.run(['git', 'merge-base', '--is-ancestor', source, 'origin/main'], cwd=root).returncode == 0:
        return {'ci_source_commit': source}
    base, _ = verify(source, app, root)
    release_format.require(subprocess.run(['git', 'merge-base', '--is-ancestor', base, 'origin/main'], cwd=root).returncode == 0, 'Hourly parent must be on main')
    return {'ci_source_commit': base}
