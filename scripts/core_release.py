#!/usr/bin/env python3
"""Build and publish the independent Core/Node release train."""
import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile

import release

ROOT = release.ROOT
REPOSITORY = release.GITHUB_REPO
TARGET = 'x86_64-unknown-linux-gnu'
VERSION = release.VERSION
TAG_PATTERN = re.compile(r'core-v(' + VERSION + r')')
CORE_ASSET = 'vibeface-core-{version}-' + TARGET + '.tar.gz'
CORE_CHECKSUM = CORE_ASSET + '.sha256'
MANIFEST = 'core-release.json'
COMMIT = release.COMMIT
BOT_EMAIL = '41898282+github-actions[bot]@users.noreply.github.com'
BUILDING_MARKER = 'BUILDING — incomplete'
FAILED_MARKER = 'FAILED — incomplete'


def require(condition, message):
    if not condition:
        raise ValueError(message)


def run(*args, cwd=None):
    return subprocess.check_output(args, cwd=cwd or ROOT, text=True).strip()


def gh(*args):
    result = subprocess.run(['gh', *args], capture_output=True, text=True)
    require(result.returncode == 0, result.stderr.strip() or 'GitHub request failed')
    return result.stdout


def parse_tag(tag):
    match = TAG_PATTERN.fullmatch(tag)
    require(match is not None, 'tag must be core-v<version>')
    value = match.group(1)
    return value, release.version(value)


def core_version(root=None):
    root = root or ROOT
    text = (root / 'apps/core/Cargo.toml').read_text(encoding='utf-8')
    match = re.search(r'^version\s*=\s*"([^"]+)"', text, re.M)
    require(match, 'Core package version is missing')
    return match.group(1)


def identity(root=None):
    root = root or ROOT
    value = core_version(root)
    release.version(value)
    source = run('git', 'rev-parse', 'HEAD', cwd=root)
    return {'app': 'core', 'version': value, 'tag': 'core-v' + value,
            'source_commit': source, 'channel': release.version(value),
            'data_epoch': release.control_constant('DATA_EPOCH'),
            'update_protocol': release.control_constant('UPDATE_PROTOCOL')}


def blob(commit, path):
    return subprocess.check_output(['git', 'show', f'{commit}:{path}'], cwd=ROOT)


def core_hourly_version(base, batch):
    require(re.fullmatch(r'[0-9]{10}', batch), 'invalid Core Hourly batch')
    datetime.datetime.strptime(batch, '%Y%m%d%H')
    current = re.search(rb'(?m)^version\s*=\s*"([^"]+)"', blob(base, 'apps/core/Cargo.toml'))[1].decode()
    value = current.split('-', 1)[0] + '-hourly.' + batch
    release.version(value)
    return value


def generated_tree(base, batch):
    value = core_hourly_version(base, batch)
    paths = ('apps/core/Cargo.toml', 'Cargo.lock')
    files = {path: blob(base, path) for path in paths}
    files['apps/core/Cargo.toml'] = re.sub(rb'(?m)^(version\s*=\s*)"[^"]+"',
                                           lambda m: m[1] + json.dumps(value).encode(),
                                           files['apps/core/Cargo.toml'], count=1)
    blocks = files['Cargo.lock'].split(b'[[package]]')
    found = 0
    for i, block in enumerate(blocks):
        if re.search(rb'(?m)^name = "vibeface-core"$', block):
            blocks[i] = re.sub(rb'(?m)^version = "[^"]+"$', b'version = ' + json.dumps(value).encode(), block, count=1)
            found += 1
    require(found == 1, 'Cargo.lock must contain one local Core package')
    files['Cargo.lock'] = b'[[package]]'.join(blocks)
    with tempfile.TemporaryDirectory() as temporary:
        index = str(Path(temporary) / 'index')
        env = dict(os.environ, GIT_INDEX_FILE=index)
        subprocess.run(['git', 'read-tree', base], cwd=ROOT, env=env, check=True)
        for path, content in files.items():
            oid = subprocess.check_output(['git', 'hash-object', '-w', '--stdin'], cwd=ROOT, env=env, input=content).decode().strip()
            subprocess.run(['git', 'update-index', '--add', '--cacheinfo', '100644', oid, path], cwd=ROOT, env=env, check=True)
        tree = subprocess.check_output(['git', 'write-tree'], cwd=ROOT, env=env).decode().strip()
    return value, tree


def verify_hourly(source):
    parts = run('git', 'rev-list', '--parents', '-n', '1', source).split()
    require(len(parts) == 2 and parts[0] == source, 'Core Hourly source must be one ordinary commit')
    base = parts[1]
    match = re.fullmatch(r'chore\(release\): core hourly ([0-9]{10})', run('git', 'show', '-s', '--format=%B', source))
    require(match, 'Core Hourly source message is invalid')
    require(run('git', 'show', '-s', '--format=%ae%n%ce', source).splitlines() == [BOT_EMAIL, BOT_EMAIL],
            'unexpected Core Hourly author')
    _, tree = generated_tree(base, match.group(1))
    require(run('git', 'rev-parse', source + '^{tree}') == tree, 'Core Hourly tree differs from expected version changes')
    return base, match.group(1)


def source_context(source):
    require(re.fullmatch(COMMIT, source), 'invalid Core source')
    if subprocess.run(['git', 'merge-base', '--is-ancestor', source, 'origin/main'], cwd=ROOT).returncode == 0:
        return {'ci_source_commit': source}
    base, _ = verify_hourly(source)
    require(subprocess.run(['git', 'merge-base', '--is-ancestor', base, 'origin/main'], cwd=ROOT).returncode == 0,
            'Core Hourly parent must be on main')
    return {'ci_source_commit': base}


def plan(tag=None, require_main=False, publishing=False, github_output=None):
    data = identity()
    if tag:
        value, channel = parse_tag(tag)
        require((tag, value, channel) == (data['tag'], data['version'], data['channel']),
                'Core tag does not match the declared version')
    if require_main or publishing:
        require(not run('git', 'status', '--porcelain'), 'Core release source must be clean')
        data.update(source_context(data['source_commit']))
        release.require_ci(data)
    if publishing:
        require(run('git', 'rev-parse', data['tag'] + '^{commit}') == data['source_commit'],
                'Core publishing requires the existing tag at this source commit')
    if github_output:
        with open(github_output, 'a') as output:
            for key, value in data.items():
                output.write(f'{key}={value}\n')
    print(json.dumps(data))
    return data


def component_info(directory, info):
    component = directory / f'vibeface-core'
    files = {component.name: release.digest(component)}
    payload = {'app': 'core', 'version': info['version'], 'tag': info['tag'],
               'source_commit': info['source_commit'], 'channel': info['channel'],
               'schema': release.SCHEMA_VERSION, 'target': TARGET,
               'data_epoch': info['data_epoch'], 'files': files}
    release.write_json(directory / 'COMPONENT-INFO.json', payload)
    return payload


def package(binary, output):
    info = identity()
    binary = Path(binary).resolve(); output = Path(output).resolve()
    require(binary.is_file(), 'Core binary is missing')
    require(run(str(binary), '--version') == 'vibeface-core ' + info['version'], 'Core binary version mismatch')
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=output, prefix='.core-') as temporary:
        root = Path(temporary) / f'vibeface-core-{info["version"]}-{TARGET}'; root.mkdir()
        shutil.copy2(binary, root / 'vibeface-core')
        component_info(root, info)
        archive = output / CORE_ASSET.format(version=info['version'])
        release.public_archive(root, archive)
    checksum = output / CORE_CHECKSUM.format(version=info['version'])
    checksum.write_text(f'{release.digest(archive)}  {archive.name}\n', encoding='utf-8', newline='\n')
    manifest = {'schema': 1, 'app': 'core', **{key: info[key] for key in ('version','tag','source_commit','channel','data_epoch','update_protocol')},
                'assets': [{'name': archive.name, 'size': archive.stat().st_size, 'sha256': release.digest(archive)}]}
    release.write_json(output / MANIFEST, manifest)
    return manifest


def verify_manifest(directory, expected=None):
    directory = Path(directory)
    manifest = json.loads((directory / MANIFEST).read_text(encoding='utf-8'))
    require(set(manifest) == {'schema','app','version','tag','source_commit','channel','data_epoch','update_protocol','assets'},
            'unsupported Core manifest fields')
    value, channel = parse_tag(manifest['tag'])
    require(manifest['app'] == 'core' and manifest['schema'] == 1 and value == manifest['version']
            and channel == manifest['channel'] and re.fullmatch(COMMIT, manifest['source_commit']),
            'invalid Core manifest identity')
    require(manifest['data_epoch'] == release.control_constant('DATA_EPOCH')
            and manifest['update_protocol'] == release.control_constant('UPDATE_PROTOCOL'),
            'Core protocol or data epoch mismatch')
    expected_name = CORE_ASSET.format(version=manifest['version'])
    require(len(manifest['assets']) == 1 and manifest['assets'][0]['name'] == expected_name,
            'Core manifest asset set mismatch')
    asset = directory / expected_name
    require(asset.is_file() and asset.stat().st_size == manifest['assets'][0]['size']
            and release.digest(asset) == manifest['assets'][0]['sha256'], 'Core asset checksum mismatch')
    checksum = directory / CORE_CHECKSUM.format(version=manifest['version'])
    require(checksum.is_file() and checksum.read_text(encoding='utf-8') ==
            f'{manifest["assets"][0]["sha256"]}  {asset.name}\n', 'Core checksum file mismatch')
    with tempfile.TemporaryDirectory() as temporary:
        root = release.unpack(asset, Path(temporary) / 'unpacked')
        info = json.loads((root / 'COMPONENT-INFO.json').read_text())
        require(info['tag'] == manifest['tag'] and info['source_commit'] == manifest['source_commit'],
                'Core component provenance mismatch')
    if expected:
        require(all(manifest[key] == value for key, value in expected.items()), 'Core manifest pin mismatch')
    return manifest


def publish(directory, repo=REPOSITORY):
    info = verify_manifest(directory)
    tag = info['tag']
    view = subprocess.run(['gh','release','view',tag,f'--repo={repo}','--json','isDraft,assets'],capture_output=True,text=True)
    if view.returncode:
        require('release not found' in view.stderr.lower(), 'cannot determine Core release state')
        subprocess.run(['gh','release','create',tag,f'--repo={repo}','--verify-tag','--draft=false','--title',tag,
                        '--notes', f'{BUILDING_MARKER}; automated Node deployment waits for the Core manifest.\n\n'
                                    f'core {info["version"]}; source {info["source_commit"]}.', '--latest=false',
                        '--prerelease=' + str(info['channel'] != 'stable').lower()], check=True)
        existing = {'isDraft': False, 'assets': []}
    else:
        existing = json.loads(view.stdout)
    remote = {asset['name']: asset for asset in existing['assets']}
    staged = {path.name: path for path in Path(directory).iterdir() if path.is_file()}
    require(set(remote) <= set(staged), 'Core release contains unexpected assets')
    for name, path in staged.items():
        if name not in remote:
            subprocess.run(['gh','release','upload',tag,f'--repo={repo}',str(path)], check=True)
    subprocess.run(['gh', 'release', 'edit', tag, f'--repo={repo}', '--notes',
                    f'core {info["version"]}; source {info["source_commit"]}.', '--latest=false',
                    '--prerelease=' + str(info['channel'] != 'stable').lower()], check=True)
    return info


def self_test():
    assert parse_tag('core-v0.0.1-hourly.2026111501') == ('0.0.1-hourly.2026111501','hourly')
    for invalid in ('v0.0.1','core-v0.0.1-nightly.1','core-v0.0.1-hourly.2026111524'):
        try: parse_tag(invalid)
        except ValueError: pass
        else: raise AssertionError('accepted invalid Core tag ' + invalid)
    print('Core release self-test passed')


def main():
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest='command', required=True)
    p = sub.add_parser('plan'); p.add_argument('--tag'); p.add_argument('--require-main', action='store_true'); p.add_argument('--publish', action='store_true'); p.add_argument('--github-output')
    p = sub.add_parser('package'); p.add_argument('--binary', type=Path, required=True); p.add_argument('--output', type=Path, required=True)
    p = sub.add_parser('verify'); p.add_argument('--directory', type=Path, required=True)
    p = sub.add_parser('publish'); p.add_argument('--directory', type=Path, required=True)
    sub.add_parser('self-test')
    args = parser.parse_args()
    if args.command == 'plan': plan(args.tag, args.require_main, args.publish, args.github_output)
    elif args.command == 'package': package(args.binary, args.output)
    elif args.command == 'verify': print(json.dumps(verify_manifest(args.directory)))
    elif args.command == 'publish': print(json.dumps(publish(args.directory)))
    else: self_test()


if __name__ == '__main__':
    main()
