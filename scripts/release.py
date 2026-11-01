#!/usr/bin/env python3
"""App release identities, immutable component archives and CLI composition."""
import argparse
import datetime
import hashlib
import gzip
import io
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import subprocess
import tarfile
import tempfile

ROOT = Path(__file__).resolve().parent.parent
APPS = ('www', 'app', 'core', 'cli', 'mobile')
TARGETS = ('aarch64-apple-darwin', 'aarch64-unknown-linux-gnu', 'x86_64-unknown-linux-gnu')
VERSION = r'(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-(nightly)\.([0-9]{8})\.([1-9][0-9]*)|-(rc)\.([1-9][0-9]*))?'
MAX_ARCHIVE = 384 * 1024 * 1024
MAX_UNPACKED = 512 * 1024 * 1024


def require(condition, message):
    if not condition:
        raise ValueError(message)


def run(*args, cwd=ROOT):
    return subprocess.check_output(args, cwd=cwd, text=True).strip()


def version(value):
    match = re.fullmatch(VERSION, value)
    require(match, 'invalid release version')
    if match[4]:
        datetime.datetime.strptime(match[5], '%Y%m%d')
    return 'nightly' if match[4] else 'rc' if match[6] else 'stable'


def parse_tag(tag):
    match = re.fullmatch(r'(www|app|core|cli|mobile)-v(.+)', tag)
    require(match, 'tag must name one of www/app/core/cli/mobile')
    app, value = match.groups()
    return app, value, version(value)


def read_version(app, root=ROOT):
    require(app in APPS, 'unknown app')
    directory = root / 'apps' / app
    if app in ('core', 'cli'):
        text = (directory / 'Cargo.toml').read_text()
        match = re.search(r'^version\s*=\s*"([^"]+)"', text, re.M)
        require(match, f'{app} must own its package version')
        return match[1]
    data = json.loads((directory / ('app.json' if app == 'mobile' else 'package.json')).read_text())
    return data['expo']['extra']['releaseVersion'] if app == 'mobile' else data['version']


def identity(app, root=ROOT):
    value = read_version(app, root)
    return {'app': app, 'version': value, 'tag': f'{app}-v{value}',
            'source_commit': run('git', 'rev-parse', 'HEAD', cwd=root), 'channel': version(value)}


def digest(path):
    h = hashlib.sha256()
    with path.open('rb') as f:
        for block in iter(lambda: f.read(1024 * 1024), b''):
            h.update(block)
    return h.hexdigest()


def safe_name(name):
    path = PurePosixPath(name)
    require(name and not path.is_absolute() and all(p not in ('', '.', '..') for p in name.split('/'))
            and re.fullmatch(r'[a-zA-Z0-9._/-]+', name), 'unsafe archive path')


def files(directory, exclude=()):
    result = {}
    for path in sorted(directory.rglob('*')):
        require(not path.is_symlink(), 'symlinks are not allowed in release assets')
        if path.is_file():
            name = path.relative_to(directory).as_posix()
            safe_name(name)
            if name not in exclude:
                result[name] = digest(path)
    require(len(result) <= 4096, 'too many release files')
    return result


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + '\n')


def public_archive(directory, output):
    def clean(info):
        require(info.isfile() or info.isdir(), 'links and special archive files are forbidden')
        info.uid = info.gid = 0
        info.uname = info.gname = ''
        info.pax_headers = {}
        return info
    output.parent.mkdir(parents=True, exist_ok=True)
    with tarfile.open(output, 'w:gz') as archive:
        archive.add(directory, arcname=directory.name, filter=clean)
    output.with_name(output.name + '.sha256').write_text(f'{digest(output)}  {output.name}\n')


def unpack(archive, destination):
    require(archive.stat().st_size <= MAX_ARCHIVE, 'archive exceeds size limit')
    with gzip.open(archive, 'rb') as compressed:
        raw = compressed.read(MAX_UNPACKED + 1)
    require(len(raw) <= MAX_UNPACKED, 'unpacked archive exceeds limit')
    with tarfile.open(fileobj=io.BytesIO(raw), mode='r:') as source:
        members = source.getmembers()
        require(0 < len(members) <= 4096, 'invalid archive entry count')
        require(sum(x.size for x in members) <= MAX_UNPACKED, 'unpacked archive exceeds limit')
        seen = set()
        roots = set()
        for item in members:
            name = item.name.rstrip('/')
            safe_name(name)
            require(name not in seen and (item.isfile() or item.isdir()), 'duplicate or special archive entry')
            roots.add(name.split('/')[0]); seen.add(name)
        require(len(roots) == 1, 'archive must contain one root')
        for item in members:
            item.mode &= 0o777
        source.extractall(destination, members=members)
    return destination / roots.pop()


def verify_component(directory, app, expected=None):
    info = json.loads((directory / 'COMPONENT-INFO.json').read_text())
    require(info['app'] == app and info['schema'] == 2, 'wrong component identity')
    require(parse_tag(info['tag'])[:2] == (app, info['version']), 'invalid component tag')
    require(re.fullmatch(r'[a-f0-9]{40}', info['source_commit']), 'invalid component source')
    if app in ('app', 'www'):
        require(not any(p.name in ('vibeface', 'vibeface-core') for p in directory.rglob('*')), 'static component must not include native programs')
    if expected:
        require(all(info[k] == v for k, v in expected.items()), 'component pin mismatch')
    require(info['files'] == files(directory, ('COMPONENT-INFO.json',)), 'component content changed')
    return info


def pack_component(app, source, output, target=None):
    info = identity(app)
    require(not run('git', 'status', '--porcelain'), 'release source must be clean')
    if app == 'core':
        require(target in TARGETS and source.is_file(), 'core requires a target binary')
        require(run(str(source.resolve()), '--version') == 'vibeface-core ' + info['version'], 'core binary version mismatch')
    else:
        require(app in ('app', 'www') and source.is_dir(), 'expected static app directory')
    name = f'vibeface-{app}-{info["version"]}' + (f'-{target}' if target else '')
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=output, prefix='.component-') as tmp:
        directory = Path(tmp) / name; directory.mkdir()
        if app == 'core':
            shutil.copy2(source, directory / 'vibeface-core')
        else:
            shutil.copytree(source, directory / ('web' if app == 'app' else 'site'))
        info.update(schema=2, target=target, files=files(directory))
        if app == 'core':
            info['data_epoch'] = control_constant('DATA_EPOCH')
        write_json(directory / 'COMPONENT-INFO.json', info)
        verify_component(directory, app)
        public_archive(directory, output / f'{name}.tar.gz')
    manifest(app, output)


def control_constant(name):
    match = re.search(rf'pub const {name}: u32 = (\d+);', (ROOT / 'crates/control/src/lib.rs').read_text())
    require(match, f'missing {name}')
    return int(match[1])


def seal_bundle(directory, core=None, app=None):
    info = identity('cli')
    require(not run('git', 'status', '--porcelain'), 'package source must be clean')
    source = info['source_commit']
    core = core or {'version': read_version('core'), 'source_commit': source}
    app = app or {'version': read_version('app'), 'source_commit': source}
    build = dict(version=info['version'], target=os.environ['PACKAGE_TARGET'], source_commit=source,
                 update_protocol=control_constant('UPDATE_PROTOCOL'), data_epoch=control_constant('DATA_EPOCH'),
                 core_version=core['version'], core_source_commit=core['source_commit'],
                 app_version=app['version'], app_source_commit=app['source_commit'])
    write_json(directory / 'BUILD-INFO.json', build)
    content = files(directory, ('FILES.sha256',))
    (directory / 'FILES.sha256').write_text(''.join(f'{value}  {name}\n' for name, value in content.items()))
    return build


def manifest(app, output):
    info = identity(app)
    assets = []
    for path in sorted(output.iterdir()):
        if path.is_file() and path.name != 'release.json':
            safe_name(path.name)
            assets.append({'name': path.name, 'size': path.stat().st_size, 'sha256': digest(path)})
    require(assets, 'release has no assets')
    info.update(schema=2, assets=assets)
    write_json(output / 'release.json', info)
    return info


def verify_manifest(directory, expected=None):
    info = json.loads((directory / 'release.json').read_text())
    require(info['schema'] == 2, 'invalid release manifest schema')
    require(parse_tag(info['tag'])[:2] == (info['app'], info['version']), 'invalid release identity')
    require(re.fullmatch(r'[a-f0-9]{40}', info['source_commit']), 'invalid release commit')
    if expected:
        require(all(info[k] == v for k, v in expected.items()), 'release identity mismatch')
    names = set()
    for asset in info['assets']:
        name = asset['name']; safe_name(name)
        require('/' not in name and name not in names, 'invalid asset name')
        path = directory / name
        require(path.is_file() and not path.is_symlink(), 'missing regular release asset')
        require(path.stat().st_size == asset['size'] and digest(path) == asset['sha256'], 'release asset checksum mismatch')
        names.add(name)
    require(names and names == {p.name for p in directory.iterdir()} - {'release.json'}, 'unexpected release assets')
    return info


def fetch_component(app, target, destination):
    lock = json.loads((ROOT / 'apps/cli/release-lock.json').read_text())[app]
    tag = lock['tag']
    require(parse_tag(tag)[0] == app, 'dependency tag names wrong component')
    require(set(lock) == {'tag'}, 'unsupported dependency lock fields')
    destination.mkdir(parents=True, exist_ok=True)
    data = json.loads(run('gh', 'release', 'view', tag, '--repo', 'leaperone/vibeface', '--json', 'isDraft,assets'))
    require(not data['isDraft'], 'dependency release is not public')
    source = json.loads(run('gh', 'api', f'repos/leaperone/vibeface/commits/{tag}'))['sha']
    release_file = destination / 'release.json'
    subprocess.run(['gh', 'release', 'download', tag, '--repo', 'leaperone/vibeface', '--pattern', 'release.json', '--dir', str(destination)], check=True)
    release = json.loads(release_file.read_text())
    require(release['source_commit'] == source and release['tag'] == tag and release['app'] == app and release['schema'] == 2,
            'dependency release source does not match tag')
    value = parse_tag(tag)[1]
    name = f'vibeface-{app}-{value}' + (f'-{target}' if app == 'core' else '') + '.tar.gz'
    assets = [a for a in release['assets'] if a['name'] == name]
    require(len(assets) == 1, 'dependency archive is missing or ambiguous')
    subprocess.run(['gh', 'release', 'download', tag, '--repo', 'leaperone/vibeface', '--pattern', name, '--dir', str(destination)], check=True)
    archive = destination / name; asset = assets[0]
    require(archive.stat().st_size == asset['size'] and digest(archive) == asset['sha256'], 'dependency archive checksum mismatch')
    directory = unpack(archive, destination / 'unpacked')
    return directory, verify_component(directory, app, {'tag': tag, 'source_commit': source})


def compose(target, binary, output):
    require(target in TARGETS, 'unsupported platform')
    require(not run('git', 'status', '--porcelain'), 'release source must be clean')
    require(run(str(binary.resolve()), '--version') == 'vibeface ' + read_version('cli'), 'CLI binary version mismatch')
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=output, prefix='.compose-') as tmp:
        tmp = Path(tmp)
        core, core_info = fetch_component('core', target, tmp / 'core')
        app, app_info = fetch_component('app', target, tmp / 'app')
        require(core_info['data_epoch'] == control_constant('DATA_EPOCH'), 'unsupported Core data epoch')
        directory = tmp / f'vibeface-{read_version("cli")}-{target}'
        directory.mkdir(); shutil.copy2(binary, directory / 'vibeface')
        shutil.copy2(core / 'vibeface-core', directory / 'vibeface-core')
        shutil.copytree(app / 'web', directory / 'web')
        (directory / 'scripts').mkdir()
        shutil.copy2(ROOT / 'scripts/install-node-service.sh', directory / 'scripts/install-node-service.sh')
        shutil.copy2(ROOT / 'README.md', directory / 'README.md')
        (directory / 'docs').mkdir()
        for name in ('node.md', 'releasing.md'):
            shutil.copy2(ROOT / 'docs' / name, directory / 'docs' / name)
        (directory / 'RUNTIME-REQUIREMENTS.md').write_text('Requires separately configured Agent programs and credentials.\n')
        os.environ['PACKAGE_TARGET'] = target
        seal_bundle(directory, core_info, app_info)
        public_archive(directory, output / f'{directory.name}.tar.gz')
    manifest('cli', output)


def publish(app, directory):
    require(not run('git', 'status', '--porcelain'), 'release source must be clean')
    info = verify_manifest(directory, identity(app))
    require(run('git', 'rev-parse', info['tag'] + '^{commit}') == info['source_commit'], 'release tag source mismatch')
    tag = info['tag']
    view = subprocess.run(['gh', 'release', 'view', tag, '--repo', 'leaperone/vibeface', '--json', 'isDraft,assets'], capture_output=True, text=True)
    if view.returncode:
        # A missing release must be distinguished from authentication/network errors.
        require('release not found' in view.stderr.lower(), 'cannot determine existing release state')
        subprocess.run(['gh', 'release', 'create', tag, '--repo', 'leaperone/vibeface', '--verify-tag', '--draft', '--title', tag, '--notes', f'{app} {info["version"]}; source {info["source_commit"]}.', '--latest=false', '--prerelease=' + str(info['channel'] != 'stable').lower()], check=True)
        existing = {'isDraft': True, 'assets': []}
    else:
        existing = json.loads(view.stdout)
    expected = {p.name: p for p in directory.iterdir() if p.is_file()}
    names = {a['name'] for a in existing['assets']}
    require(names <= set(expected), 'release contains unexpected assets')
    with tempfile.TemporaryDirectory(dir=directory.parent, prefix='.published-') as tmp:
        tmp = Path(tmp)
        for name in names:
            subprocess.run(['gh', 'release', 'download', tag, '--repo', 'leaperone/vibeface', '--pattern', name, '--dir', str(tmp)], check=True)
            require(digest(tmp / name) == digest(expected[name]), 'existing published bytes differ')
        missing = set(expected) - names
        require(existing['isDraft'] or not missing, 'cannot add to a public release')
        if missing:
            subprocess.run(['gh', 'release', 'upload', tag, '--repo', 'leaperone/vibeface', *[str(expected[n]) for n in sorted(missing)]], check=True)
        for name in missing:
            subprocess.run(['gh', 'release', 'download', tag, '--repo', 'leaperone/vibeface', '--pattern', name, '--dir', str(tmp)], check=True)
            require(digest(tmp / name) == digest(expected[name]), 'uploaded bytes differ')
    if existing['isDraft']:
        subprocess.run(['gh', 'release', 'edit', tag, '--repo', 'leaperone/vibeface', '--draft=false', '--latest=false', '--prerelease=' + str(info['channel'] != 'stable').lower()], check=True)


def self_test():
    for app in APPS:
        assert parse_tag(f'{app}-v0.0.1-nightly.20261101.1') == (app, '0.0.1-nightly.20261101.1', 'nightly')
    for tag in ('v0.0.1', 'other-v0.0.1', 'cli-v01.0.1', 'core-v0.0.1-nightly.20260229.1', 'app-v1.0.0+extra'):
        try:
            parse_tag(tag)
        except ValueError:
            pass
        else:
            raise AssertionError(f'accepted invalid tag {tag}')
    scratch = ROOT / '.artifacts/release-tests'; scratch.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=scratch) as tmp:
        tmp = Path(tmp); directory = tmp / 'component'; directory.mkdir()
        (directory / 'test').write_text('known content')
        public_archive(directory, tmp / 'asset.tar.gz')
        extracted = unpack(tmp / 'asset.tar.gz', tmp / 'out')
        assert (extracted / 'test').read_text() == 'known content'
        info = dict(app='app', schema=2, tag='app-v0.0.1', version='0.0.1',
                    source_commit='a' * 40, files=files(extracted))
        write_json(extracted / 'COMPONENT-INFO.json', info)
        assert verify_component(extracted, 'app', {'source_commit': 'a' * 40}) == info
        for expected in ({'source_commit': 'b' * 40}, {'tag': 'app-v0.0.2'}):
            try:
                verify_component(extracted, 'app', expected)
            except ValueError:
                pass
            else:
                raise AssertionError('accepted wrong component pin')
        (extracted / 'test').write_text('tampered')
        try:
            verify_component(extracted, 'app')
        except ValueError:
            pass
        else:
            raise AssertionError('accepted tampered component')

        try:
            safe_name('../escape')
        except ValueError:
            pass
        else:
            raise AssertionError('accepted traversal')
    print('release identities and archive self-tests passed')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='command', required=True)
    p = sub.add_parser('plan'); p.add_argument('--app', choices=APPS); p.add_argument('--tag'); p.add_argument('--github-output'); p.add_argument('--require-main', action='store_true')
    p = sub.add_parser('component'); p.add_argument('app', choices=('core', 'app', 'www')); p.add_argument('--input', type=Path, required=True); p.add_argument('--output', type=Path, required=True); p.add_argument('--target')
    p = sub.add_parser('seal-bundle'); p.add_argument('directory', type=Path)
    p = sub.add_parser('manifest'); p.add_argument('app', choices=APPS); p.add_argument('directory', type=Path)
    p = sub.add_parser('compose'); p.add_argument('--target', required=True); p.add_argument('--binary', type=Path, required=True); p.add_argument('--output', type=Path, required=True)
    p = sub.add_parser('publish'); p.add_argument('app', choices=APPS); p.add_argument('directory', type=Path)
    sub.add_parser('self-test')
    args = parser.parse_args()
    if args.command == 'plan':
        app = parse_tag(args.tag)[0] if args.tag else args.app
        require(app in APPS, 'specify an app or tag')
        data = identity(app)
        if os.environ.get('GITHUB_EVENT_NAME') == 'workflow_dispatch':
            require(os.environ.get('GITHUB_REF') == 'refs/tags/' + data['tag'], 'dispatch ref must match release tag')
        require(not args.tag or args.tag == data['tag'], 'tag does not match app version')
        if app == 'mobile':
            expo = json.loads((ROOT / 'apps/mobile/app.json').read_text())['expo']
            require(expo['version'] == data['version'].split('-', 1)[0], 'mobile marketing version mismatch')
        if args.require_main:
            subprocess.run(['git', 'merge-base', '--is-ancestor', 'HEAD', 'origin/main'], check=True)
            require(not run('git', 'status', '--porcelain'), 'release source must be clean')
        if args.github_output:
            with open(args.github_output, 'a') as f:
                for key, value in data.items(): f.write(f'{key}={value}\n')
        print(json.dumps(data))
    elif args.command == 'component': pack_component(args.app, args.input, args.output, args.target)
    elif args.command == 'seal-bundle': seal_bundle(args.directory)
    elif args.command == 'manifest': manifest(args.app, args.directory)
    elif args.command == 'compose': compose(args.target, args.binary, args.output)
    elif args.command == 'publish': publish(args.app, args.directory)
    else: self_test()


if __name__ == '__main__':
    main()
