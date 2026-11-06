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
import sys
import tarfile
import tempfile

sys.dont_write_bytecode = True

ROOT = Path(__file__).resolve().parent.parent
APPS = ('www', 'app', 'core', 'cli', 'mobile')
PRODUCT_APPS = ('app', 'core', 'cli', 'mobile')
RELEASES = ('product', 'www')
TARGETS = ('aarch64-apple-darwin', 'aarch64-unknown-linux-gnu', 'x86_64-unknown-linux-gnu')
VERSION = r'(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-(nightly)\.([0-9]{8})\.([1-9][0-9]*)|-(rc)\.([1-9][0-9]*))?'
MAX_ARCHIVE = 384 * 1024 * 1024
MAX_UNPACKED = 512 * 1024 * 1024
SCHEMA_VERSION = 1
GITHUB_REPO = 'leaperone/vibeface'


def require(condition, message):
    if not condition:
        raise ValueError(message)


def run(*args, cwd=None):
    return subprocess.check_output(args, cwd=cwd or ROOT, text=True).strip()


def version(value):
    match = re.fullmatch(VERSION, value)
    require(match, 'invalid release version')
    if match[4]:
        datetime.datetime.strptime(match[5], '%Y%m%d')
    return 'nightly' if match[4] else 'rc' if match[7] else 'stable'


def parse_tag(tag):
    m = re.fullmatch(r'www-v(' + VERSION + r')', tag)
    if m:
        value = m.group(1)
        return 'www', value, version(value)
    m = re.fullmatch(r'v(' + VERSION + r')', tag)
    if m:
        value = m.group(1)
        return 'product', value, version(value)
    raise ValueError('tag must be product v<version> or www-v<version>')


def read_version(app, root=None):
    root = root or ROOT
    require(app in APPS, 'unknown app')
    directory = root / 'apps' / app
    if app in ('core', 'cli'):
        text = (directory / 'Cargo.toml').read_text()
        match = re.search(r'^version\s*=\s*"([^"]+)"', text, re.M)
        require(match, f'{app} must own its package version')
        return match[1]
    data = json.loads((directory / ('app.json' if app == 'mobile' else 'package.json')).read_text())
    return data['expo']['extra']['releaseVersion'] if app == 'mobile' else data['version']


def canonical_product_version(root=None):
    return read_version('cli', root)


def require_product_version_alignment(root=None):
    v = canonical_product_version(root)
    for app in ('core', 'app', 'mobile'):
        require(read_version(app, root) == v, f'product app {app} version must equal CLI version')
    return v


def release_tag_for(release_app, version_value):
    require(release_app in RELEASES, 'unknown release app')
    return f'v{version_value}' if release_app == 'product' else f'www-v{version_value}'


def identity_release(release_app, root=None, source_commit=None):
    root = root or ROOT
    require(release_app in RELEASES, 'unknown release app')
    if release_app == 'product':
        value = require_product_version_alignment(root)
    else:
        value = read_version('www', root)
    ch = version(value)
    tag = release_tag_for(release_app, value)
    return {'app': release_app, 'version': value, 'tag': tag, 'source_commit': source_commit or run('git', 'rev-parse', 'HEAD', cwd=root), 'channel': ch}


def identity(app, root=None):
    """Component or mobile identity; release manifests use identity_release."""
    root = root or ROOT
    value = read_version(app, root)
    if app == 'mobile':
        tag = f'v{value}'
    elif app in ('app', 'core'):
        tag = f'v{value}'
    elif app == 'www':
        tag = f'www-v{value}'
    else:
        tag = f'v{value}'
    return {'app': app, 'version': value, 'tag': tag,
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


def expected_release_kind_for_component(app):
    return 'www' if app == 'www' else 'product'


def verify_component(directory, app, expected=None):
    info = json.loads((directory / 'COMPONENT-INFO.json').read_text())
    fields = {'app', 'version', 'tag', 'source_commit', 'channel', 'schema', 'target', 'files'}
    if app == 'core':
        fields.add('data_epoch')
    require(set(info) == fields, 'unsupported component fields')
    require(info['app'] == app and type(info['schema']) is int and info['schema'] == SCHEMA_VERSION, 'wrong component identity')
    rk, v, ch = parse_tag(info['tag'])
    require(rk == expected_release_kind_for_component(app) and v == info['version'] and ch == info['channel'], 'invalid component tag')
    require(re.fullmatch(r'[a-f0-9]{40}', info['source_commit']), 'invalid component source')
    if app == 'core':
        require(type(info['data_epoch']) is int and info['data_epoch'] == control_constant('DATA_EPOCH') and info['target'] in TARGETS,
                'unsupported Core data epoch or target')
    else:
        require(info['target'] is None, 'static component must not specify a native target')
    if app in ('app', 'www'):
        require(not any(p.name in ('vibeface', 'vibeface-core') for p in directory.rglob('*')), 'static component must not include native programs')
    if expected:
        require(all(info[k] == v for k, v in expected.items()), 'component pin mismatch')
    require(info['files'] == files(directory, ('COMPONENT-INFO.json',)), 'component content changed')
    return info


def component_release_tag(app, version_value):
    return f'www-v{version_value}' if app == 'www' else f'v{version_value}'


def pack_component(app, source, output, target=None):
    info = identity(app)
    require(not run('git', 'status', '--porcelain'), 'release source must be clean')
    tag = component_release_tag(app, info['version'])
    info['tag'] = tag
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
        info.update(schema=SCHEMA_VERSION, target=target, files=files(directory))
        if app == 'core':
            info['data_epoch'] = control_constant('DATA_EPOCH')
        write_json(directory / 'COMPONENT-INFO.json', info)
        verify_component(directory, app)
        public_archive(directory, output / f'{name}.tar.gz')
    manifest_release(expected_release_kind_for_component(app), output)


def control_constant(name):
    match = re.search(rf'pub const {name}: u32 = (\d+);', (ROOT / 'crates/control/src/lib.rs').read_text())
    require(match, f'missing {name}')
    return int(match[1])


def verify_build_info(info):
    require(set(info) == {'version', 'target', 'source_commit', 'update_protocol', 'data_epoch',
                          'core_version', 'core_source_commit', 'app_version', 'app_source_commit',
                          'component_source_commits'},
            'unsupported build info fields')
    require(info['target'] in TARGETS, 'unsupported build target')
    for key, constant in (('update_protocol', 'UPDATE_PROTOCOL'), ('data_epoch', 'DATA_EPOCH')):
        require(type(info[key]) is int and info[key] == control_constant(constant), 'unsupported build protocol or data epoch')
    for key in ('source_commit', 'core_source_commit', 'app_source_commit'):
        require(re.fullmatch(r'[a-f0-9]{40}', info[key]), 'invalid build source')
    csc = info['component_source_commits']
    require(isinstance(csc, dict) and set(csc) == {'app', 'core'} and all(re.fullmatch(r'[a-f0-9]{40}', v) for v in csc.values()),
            'invalid component_source_commits')
    channel = version(info['version'])
    require(all(version(info[key]) == channel for key in ('core_version', 'app_version')),
            'CLI, Core and App must use the same release channel')


def seal_bundle(directory, core=None, app=None):
    info = identity('cli')
    require(not run('git', 'status', '--porcelain'), 'package source must be clean')
    source = info['source_commit']
    core = core or {'version': read_version('core'), 'source_commit': source}
    app = app or {'version': read_version('app'), 'source_commit': source}
    build = dict(version=info['version'], target=os.environ['PACKAGE_TARGET'], source_commit=source,
                 update_protocol=control_constant('UPDATE_PROTOCOL'), data_epoch=control_constant('DATA_EPOCH'),
                 core_version=core['version'], core_source_commit=core['source_commit'],
                 app_version=app['version'], app_source_commit=app['source_commit'],
                 component_source_commits={'core': core['source_commit'], 'app': app['source_commit']})
    verify_build_info(build)
    write_json(directory / 'BUILD-INFO.json', build)
    content = files(directory, ('FILES.sha256',))
    (directory / 'FILES.sha256').write_text(''.join(f'{value}  {name}\n' for name, value in content.items()))
    return build


def manifest_release(release_app, output, source_commit=None):
    require(release_app in RELEASES, 'manifest requires product or www release app')
    info = identity_release(release_app, source_commit=source_commit)
    assets = []
    for path in sorted(output.iterdir()):
        if path.is_file() and path.name != 'release.json':
            safe_name(path.name)
            assets.append({'name': path.name, 'size': path.stat().st_size, 'sha256': digest(path)})
    require(assets, 'release has no assets')
    info.update(schema=SCHEMA_VERSION, assets=assets)
    write_json(output / 'release.json', info)
    return info


def manifest(app, output):
    """Backward-compatible name: app is release kind product|www or legacy mobile."""
    if app in RELEASES:
        return manifest_release(app, output)
    require(app == 'mobile', 'use manifest_release for product/www')
    info = identity('mobile')
    assets = []
    for path in sorted(output.iterdir()):
        if path.is_file() and path.name != 'release.json':
            safe_name(path.name)
            assets.append({'name': path.name, 'size': path.stat().st_size, 'sha256': digest(path)})
    require(assets, 'release has no assets')
    info.update(schema=SCHEMA_VERSION, assets=assets)
    write_json(output / 'release.json', info)
    return info


def verify_release_identity(info):
    require(set(info) == {'app', 'version', 'tag', 'source_commit', 'channel', 'schema', 'assets'},
            'unsupported release manifest fields')
    require(type(info['schema']) is int and info['schema'] == SCHEMA_VERSION, 'invalid release manifest schema')
    require(info['app'] in RELEASES or info['app'] == 'mobile', 'invalid release manifest app')
    if info['app'] == 'mobile':
        require(info['tag'] == f'v{info["version"]}', 'invalid mobile release tag')
        require(version(info['version']) == info['channel'], 'invalid mobile release channel')
    else:
        rk, v, ch = parse_tag(info['tag'])
        require(rk == info['app'] and v == info['version'] and ch == info['channel'], 'invalid release identity')
    require(re.fullmatch(r'[a-f0-9]{40}', info['source_commit']), 'invalid release commit')


def verify_manifest(directory, expected=None):
    info = json.loads((directory / 'release.json').read_text())
    verify_release_identity(info)
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


def _read_native_info(native_dir, target):
    path = native_dir / 'NATIVE-INFO.json'
    require(path.is_file(), 'missing NATIVE-INFO.json')
    n = json.loads(path.read_text())
    require(set(n) == {'schema', 'target', 'version', 'source_commit', 'files', 'data_epoch'}, 'unsupported native info fields')
    require(n['schema'] == SCHEMA_VERSION and n['target'] == target, 'native info target or schema mismatch')
    require(n['data_epoch'] == control_constant('DATA_EPOCH'), 'native info data epoch mismatch')
    require(re.fullmatch(r'[a-f0-9]{40}', n['source_commit']), 'invalid native source commit')
    require(set(n['files']) == {'vibeface', 'vibeface-core'}, 'native info must list vibeface and vibeface-core')
    return n


def stage_native(target, binary_directory, output):
    require(target in TARGETS, 'unsupported platform')
    binary_directory = binary_directory.resolve()
    require(binary_directory.is_dir(), 'native input must be a directory')
    cli_bin = binary_directory / 'vibeface'
    core_bin = binary_directory / 'vibeface-core'
    require(cli_bin.is_file() and core_bin.is_file(), 'native directory must contain vibeface and vibeface-core')
    pv = canonical_product_version()
    require(run(str(cli_bin), '--version') == f'vibeface {pv}', 'CLI binary version mismatch')
    require(run(str(core_bin), '--version') == f'vibeface-core {pv}', 'core binary version mismatch')
    require(not run('git', 'status', '--porcelain'), 'release source must be clean')
    source_commit = run('git', 'rev-parse', 'HEAD')
    output.mkdir(parents=True, exist_ok=True)
    shutil.copy2(cli_bin, output / 'vibeface')
    shutil.copy2(core_bin, output / 'vibeface-core')
    files_map = files(output, ('NATIVE-INFO.json',))
    write_json(output / 'NATIVE-INFO.json', dict(
        schema=SCHEMA_VERSION, target=target, version=pv, source_commit=source_commit,
        files=files_map, data_epoch=control_constant('DATA_EPOCH')))


def compose_offline(target, native_dir, app_archive, output):
    require(target in TARGETS, 'unsupported platform')
    native_dir = native_dir.resolve()
    app_archive = app_archive.resolve()
    output.mkdir(parents=True, exist_ok=True)
    ninfo = _read_native_info(native_dir, target)
    pv = canonical_product_version()
    require(ninfo['version'] == pv, 'native staging version must match CLI product version')
    with tempfile.TemporaryDirectory(dir=output, prefix='.compose-') as tmp:
        tmp = Path(tmp)
        unpacked_app = unpack(app_archive, tmp / 'app-unpack')
        app_ci = verify_component(unpacked_app, 'app')
        require(app_ci['version'] == pv and app_ci['source_commit'] == ninfo['source_commit'],
                'app archive version or source must match native staging')
        cli_src = native_dir / 'vibeface'
        core_src = native_dir / 'vibeface-core'
        require(cli_src.is_file() and core_src.is_file(), 'native dir must contain binaries')
        require(digest(cli_src) == ninfo['files']['vibeface'] and digest(core_src) == ninfo['files']['vibeface-core'],
                'native binaries do not match NATIVE-INFO')
        directory = tmp / f'vibeface-{pv}-{target}'
        directory.mkdir()
        shutil.copy2(cli_src, directory / 'vibeface')
        shutil.copy2(core_src, directory / 'vibeface-core')
        shutil.copytree(unpacked_app / 'web', directory / 'web')
        (directory / 'scripts').mkdir()
        shutil.copy2(ROOT / 'scripts/install-node-service.sh', directory / 'scripts/install-node-service.sh')
        shutil.copy2(ROOT / 'README.md', directory / 'README.md')
        (directory / 'docs').mkdir()
        for name in ('node.md', 'releasing.md'):
            shutil.copy2(ROOT / 'packages/docs' / name, directory / 'docs' / name)
        (directory / 'RUNTIME-REQUIREMENTS.md').write_text('Requires separately configured Agent programs and credentials.\n')
        os.environ['PACKAGE_TARGET'] = target
        core_info = {'version': pv, 'source_commit': ninfo['source_commit']}
        app_info = {'version': app_ci['version'], 'source_commit': app_ci['source_commit']}
        seal_bundle(directory, core_info, app_info)
        bundle_name = f'vibeface-{pv}-{target}'
        public_archive(directory, tmp / f'{bundle_name}.tar.gz')
        core_only = tmp / f'vibeface-core-only-{pv}-{target}'
        core_only.mkdir()
        shutil.copy2(core_src, core_only / 'vibeface-core')
        core_ci = dict(app='core', version=pv, tag=f'v{pv}', source_commit=ninfo['source_commit'],
                       channel=version(pv), schema=SCHEMA_VERSION, target=target,
                       data_epoch=control_constant('DATA_EPOCH'))
        write_json(core_only / 'COMPONENT-INFO.json', core_ci)
        core_ci['files'] = files(core_only, ('COMPONENT-INFO.json',))
        write_json(core_only / 'COMPONENT-INFO.json', core_ci)
        verify_component(core_only, 'core')
        core_archive_name = f'vibeface-core-{pv}-{target}'
        public_archive(core_only, tmp / f'{core_archive_name}.tar.gz')
        for p in (tmp / f'{bundle_name}.tar.gz', tmp / f'{bundle_name}.tar.gz.sha256',
                  tmp / f'{core_archive_name}.tar.gz', tmp / f'{core_archive_name}.tar.gz.sha256'):
            shutil.copy2(p, output / p.name)
        manifest_release('product', output, source_commit=ninfo['source_commit'])


def mobile_provenance(apk):
    expo = json.loads((ROOT / 'apps/mobile/app.json').read_text())['expo']
    candidates = sorted((Path(os.environ.get('ANDROID_HOME', '/nonexistent')) / 'build-tools').glob('*/aapt'))
    tool = shutil.which('aapt') or (str(candidates[-1]) if candidates else None)
    require(tool, 'Android aapt is required to verify APK identity')
    badging = run(tool, 'dump', 'badging', str(apk.resolve()))
    match = re.search(r"^package: name='([^']+)' versionCode='([0-9]+)' versionName='([^']+)'", badging, re.M)
    require(match, 'APK has no Android package identity')
    package, code, marketing = match.groups()
    require((package, int(code), marketing) == (expo['android']['package'], expo['android']['versionCode'], expo['version']),
            'APK package, versionCode or marketing version mismatch')
    return dict(version=read_version('mobile'), sourceCommit=run('git', 'rev-parse', 'HEAD'), sha256=digest(apk),
                applicationId=package, versionCode=int(code), versionName=marketing)


def _expected_mobile_assets(version_value):
    name = f'vibeface-{version_value}-android.apk'
    return {name, name + '.sha256', name + '.provenance.json'}


def _expected_www_assets(version_value):
    name = f'vibeface-www-{version_value}.tar.gz'
    return {name, name + '.sha256', 'release.json'}


def _product_cli_core_names(version_value):
    names = set()
    for t in TARGETS:
        b = f'vibeface-{version_value}-{t}'
        names.add(b + '.tar.gz')
        names.add(b + '.tar.gz.sha256')
        c = f'vibeface-core-{version_value}-{t}'
        names.add(c + '.tar.gz')
        names.add(c + '.tar.gz.sha256')
    return names


def _expected_product_assets(version_value):
    names = _product_cli_core_names(version_value)
    names |= _expected_mobile_assets(version_value)
    names.add(f'vibeface-app-{version_value}.tar.gz')
    names.add(f'vibeface-app-{version_value}.tar.gz.sha256')
    names.add('release.json')
    return names


def verify_public(release_app, destination, repo=GITHUB_REPO):
    require(release_app in RELEASES or release_app == 'mobile', 'verify-public supports product, www, or mobile')
    if release_app == 'mobile':
        expected = identity('mobile')
        tag = expected['tag']
    elif release_app == 'product':
        expected = identity_release('product')
        tag = expected['tag']
    else:
        expected = identity_release('www')
        tag = expected['tag']
    view = json.loads(run('gh', 'release', 'view', tag, f'--repo={repo}', '--json', 'isDraft,assets'))
    require(not view['isDraft'], 'release is not public')
    source = json.loads(run('gh', 'api', f'repos/{repo}/commits/{tag}'))['sha']
    require(source == expected['source_commit'], 'public tag source mismatch')
    require(not destination.exists(), 'download destination already exists')
    destination.mkdir(parents=True)
    subprocess.run(['gh', 'release', 'download', tag, f'--repo={repo}', '--dir', str(destination)], check=True)
    info = verify_manifest(destination, expected)
    public_names = {a['name'] for a in view['assets']}
    if release_app == 'www':
        names = _expected_www_assets(expected['version'])
        require(public_names == names, 'public www asset set mismatch')
        require({a['name'] for a in info['assets']} == names - {'release.json'}, 'www manifest asset set mismatch')
        directory = unpack(destination / f'vibeface-www-{expected["version"]}.tar.gz', destination.parent / (destination.name + '-www-unpack'))
        verify_component(directory, 'www', dict(expected, tag=tag))
    elif release_app == 'product':
        names = _expected_product_assets(expected['version'])
        require(public_names == names, 'public product asset set mismatch')
        require({a['name'] for a in info['assets']} == names - {'release.json'}, 'product manifest asset set mismatch')
        app_arc = destination / f'vibeface-app-{expected["version"]}.tar.gz'
        app_dir = unpack(app_arc, destination.parent / (destination.name + '-app'))
        verify_component(app_dir, 'app', {'version': expected['version'], 'source_commit': source, 'tag': tag})
        apk_name = f'vibeface-{expected["version"]}-android.apk'
        require(json.loads((destination / (apk_name + '.provenance.json')).read_text()) == mobile_provenance(destination / apk_name),
                'APK provenance mismatch')
        for index, target in enumerate(TARGETS):
            cli_arc = destination / f'vibeface-{expected["version"]}-{target}.tar.gz'
            cli_dir = unpack(cli_arc, destination.parent / (destination.name + f'-cli-{index}'))
            build = json.loads((cli_dir / 'BUILD-INFO.json').read_text())
            verify_build_info(build)
            require(build['version'] == expected['version'] and build['source_commit'] == source and build['target'] == target,
                    'CLI bundle identity mismatch')
            require(build['component_source_commits']['app'] == build['app_source_commit'] and build['component_source_commits']['core'] == build['core_source_commit'],
                    'component_source_commits mismatch')
            pins = dict(version=expected['version'], source_commit=source, target=target,
                        core_version=expected['version'], app_version=expected['version'],
                        core_source_commit=source, app_source_commit=source)
            require(all(build.get(k) == v for k, v in pins.items()), 'CLI build pin mismatch')
            content = files(cli_dir, ('FILES.sha256',))
            require((cli_dir / 'FILES.sha256').read_text() == ''.join(f'{value}  {name}\n' for name, value in content.items()),
                    'CLI bundled content changed')
            core_arc = destination / f'vibeface-core-{expected["version"]}-{target}.tar.gz'
            core_dir = unpack(core_arc, destination.parent / (destination.name + f'-core-{index}'))
            verify_component(core_dir, 'core', {'version': expected['version'], 'source_commit': source, 'target': target, 'tag': tag})
    else:
        names = _expected_mobile_assets(expected['version'])
        require(public_names == names | {'release.json'}, 'public mobile asset set mismatch')
        require({a['name'] for a in info['assets']} == names - {'release.json'}, 'mobile manifest asset set mismatch')
        apk_only = next(n for n in names if n.endswith('.apk'))
        require(json.loads((destination / (apk_only + '.provenance.json')).read_text()) ==
                mobile_provenance(destination / apk_only), 'APK provenance mismatch')
    return info


def _git_head_clean():
    return not run('git', 'status', '--porcelain')


def _publish_common(release_app, directory, repo, require_tag_head, require_main_ancestor):
    require(release_app in RELEASES, 'publish supports product or www')
    if require_tag_head or require_main_ancestor:
        require(_git_head_clean(), 'release source must be clean')
    info = verify_manifest(directory, identity_release(release_app) if require_tag_head else None)
    if require_tag_head:
        require(run('git', 'rev-parse', info['tag'] + '^{commit}') == info['source_commit'], 'release tag source mismatch')
        require(run('git', 'rev-parse', 'HEAD') == info['source_commit'], 'HEAD must match release source commit')
    if require_main_ancestor:
        subprocess.run(['git', 'merge-base', '--is-ancestor', info['source_commit'], 'origin/main'], check=True)
    tag = info['tag']
    view = subprocess.run(['gh', 'release', 'view', tag, f'--repo={repo}', '--json', 'isDraft,assets'], capture_output=True, text=True)
    if view.returncode:
        require('release not found' in view.stderr.lower(), 'cannot determine existing release state')
        subprocess.run(['gh', 'release', 'create', tag, f'--repo={repo}', '--verify-tag', '--draft', '--title', tag,
                        '--notes', f'{release_app} {info["version"]}; source {info["source_commit"]}.', '--latest=false',
                        '--prerelease=' + str(info['channel'] != 'stable').lower()], check=True)
        existing = {'isDraft': True, 'assets': []}
    else:
        existing = json.loads(view.stdout)
    expected = {p.name: p for p in directory.iterdir() if p.is_file()}
    names = {a['name'] for a in existing['assets']}
    require(names <= set(expected), 'release contains unexpected assets')
    with tempfile.TemporaryDirectory(dir=directory.parent, prefix='.published-') as tmp:
        tmp = Path(tmp)
        for name in names:
            subprocess.run(['gh', 'release', 'download', tag, f'--repo={repo}', '--pattern', name, '--dir', str(tmp)], check=True)
            require(digest(tmp / name) == digest(expected[name]), 'existing published bytes differ')
        missing = set(expected) - names
        require(existing['isDraft'] or not missing, 'cannot add to a public release')
        if missing:
            subprocess.run(['gh', 'release', 'upload', tag, f'--repo={repo}', *[str(expected[n]) for n in sorted(missing)]], check=True)
        for name in missing:
            subprocess.run(['gh', 'release', 'download', tag, f'--repo={repo}', '--pattern', name, '--dir', str(tmp)], check=True)
            require(digest(tmp / name) == digest(expected[name]), 'uploaded bytes differ')
    if existing['isDraft']:
        subprocess.run(['gh', 'release', 'edit', tag, f'--repo={repo}', '--draft=false', '--latest=false',
                        '--prerelease=' + str(info['channel'] != 'stable').lower()], check=True)


def publish(release_app, directory, repo=GITHUB_REPO, require_tag_head=True, require_main_ancestor=True):
    _publish_common(release_app, directory, repo, require_tag_head, require_main_ancestor)


def publish_mobile(directory, repo=GITHUB_REPO):
    require(_git_head_clean(), 'release source must be clean')
    info = verify_manifest(directory, identity('mobile'))
    require(run('git', 'rev-parse', info['tag'] + '^{commit}') == info['source_commit'], 'release tag source mismatch')
    tag = info['tag']
    view = subprocess.run(['gh', 'release', 'view', tag, f'--repo={repo}', '--json', 'isDraft,assets'], capture_output=True, text=True)
    if view.returncode:
        require('release not found' in view.stderr.lower(), 'cannot determine existing release state')
        subprocess.run(['gh', 'release', 'create', tag, f'--repo={repo}', '--verify-tag', '--draft', '--title', tag,
                        '--notes', f'mobile {info["version"]}; source {info["source_commit"]}.', '--latest=false',
                        '--prerelease=' + str(info['channel'] != 'stable').lower()], check=True)
        existing = {'isDraft': True, 'assets': []}
    else:
        existing = json.loads(view.stdout)
    expected = {p.name: p for p in directory.iterdir() if p.is_file()}
    names = {a['name'] for a in existing['assets']}
    require(names <= set(expected), 'release contains unexpected assets')
    with tempfile.TemporaryDirectory(dir=directory.parent, prefix='.published-') as tmp:
        tmp = Path(tmp)
        for name in names:
            subprocess.run(['gh', 'release', 'download', tag, f'--repo={repo}', '--pattern', name, '--dir', str(tmp)], check=True)
            require(digest(tmp / name) == digest(expected[name]), 'existing published bytes differ')
        missing = set(expected) - names
        require(existing['isDraft'] or not missing, 'cannot add to a public release')
        if missing:
            subprocess.run(['gh', 'release', 'upload', tag, f'--repo={repo}', *[str(expected[n]) for n in sorted(missing)]], check=True)
        for name in missing:
            subprocess.run(['gh', 'release', 'download', tag, f'--repo={repo}', '--pattern', name, '--dir', str(tmp)], check=True)
            require(digest(tmp / name) == digest(expected[name]), 'uploaded bytes differ')
    if existing['isDraft']:
        subprocess.run(['gh', 'release', 'edit', tag, f'--repo={repo}', '--draft=false', '--latest=false',
                        '--prerelease=' + str(info['channel'] != 'stable').lower()], check=True)


def _partial_manifest_identity(path):
    require(path.is_file(), 'missing partial release.json')
    data = json.loads(path.read_text())
    verify_release_identity(data)
    return data


def assemble_product(input_root, output):
    input_root = input_root.resolve()
    pv = canonical_product_version()
    tag_p = f'v{pv}'
    # Validate tree layout and identities
    app_dir = input_root / 'app'
    native_root = input_root / 'native'
    mobile_dir = input_root / 'mobile'
    require(app_dir.is_dir() and native_root.is_dir() and mobile_dir.is_dir(), 'input-root must contain app, native/, mobile/')
    extras = set(p.name for p in input_root.iterdir()) - {'app', 'native', 'mobile'}
    require(not extras, 'unexpected paths under input-root')
    for t in TARGETS:
        nd = native_root / t
        require(nd.is_dir(), f'missing native/{t}')
    # App
    app_part = _partial_manifest_identity(app_dir / 'release.json')
    require(app_part['app'] == 'product' and app_part['version'] == pv, 'app producer identity mismatch')
    require({p.name for p in app_dir.iterdir() if p.is_file()} == {f'vibeface-app-{pv}.tar.gz', f'vibeface-app-{pv}.tar.gz.sha256', 'release.json'},
            'app producer file set invalid')
    app_arc = app_dir / f'vibeface-app-{pv}.tar.gz'
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        ad = unpack(app_arc, tmp / 'app-u')
        verify_component(ad, 'app', {'version': pv, 'source_commit': app_part['source_commit'], 'tag': tag_p})
    # Mobile
    mob_part = _partial_manifest_identity(mobile_dir / 'release.json')
    require(mob_part['app'] == 'mobile' and mob_part['version'] == pv, 'mobile producer identity mismatch')
    apk_name = f'vibeface-{pv}-android.apk'
    mob_files = {apk_name, apk_name + '.sha256', apk_name + '.provenance.json', 'release.json'}
    require({p.name for p in mobile_dir.iterdir() if p.is_file()} == mob_files, 'mobile producer file set invalid')
    # Per-target compose outputs
    src_commit = app_part['source_commit']
    seen_hashes = {}
    for target in TARGETS:
        cdir = input_root / 'native' / target
        part = _partial_manifest_identity(cdir / 'release.json')
        require(part['app'] == 'product' and part['version'] == pv and part['source_commit'] == src_commit,
                'native compose identity mismatch')
        cli_n = f'vibeface-{pv}-{target}.tar.gz'
        core_n = f'vibeface-core-{pv}-{target}.tar.gz'
        require({p.name for p in cdir.iterdir() if p.is_file()} == {cli_n, cli_n + '.sha256', core_n, core_n + '.sha256', 'release.json'},
                f'native {target} file set invalid')
        for base in (cli_n, core_n):
            h = digest(cdir / base)
            require(h not in seen_hashes or seen_hashes[h] == base, 'duplicate staged bytes')
            seen_hashes[h] = base
        with tempfile.TemporaryDirectory() as tmp:
            tmp = Path(tmp)
            cli_dir = unpack(cdir / cli_n, tmp / 'cli')
            build = json.loads((cli_dir / 'BUILD-INFO.json').read_text())
            verify_build_info(build)
            require(build['target'] == target and build['version'] == pv and build['source_commit'] == src_commit, 'CLI bundle mismatch')
            require(build['component_source_commits']['app'] == app_part['source_commit'], 'compose app source mismatch')
            require(build['component_source_commits']['core'] == src_commit, 'compose core source mismatch')
            core_dir = unpack(cdir / core_n, tmp / 'core')
            verify_component(core_dir, 'core', {'version': pv, 'source_commit': src_commit, 'target': target, 'tag': tag_p})
    # APK provenance
    require(json.loads((mobile_dir / (apk_name + '.provenance.json')).read_text()) == mobile_provenance(mobile_dir / apk_name),
            'APK provenance mismatch')
    # Assemble output
    output.mkdir(parents=True, exist_ok=True)
    if any(output.iterdir()):
        raise ValueError('output directory must be empty')
    for name in (f'vibeface-app-{pv}.tar.gz', f'vibeface-app-{pv}.tar.gz.sha256'):
        shutil.copy2(app_dir / name, output / name)
    for name in mob_files:
        if name == 'release.json':
            continue
        shutil.copy2(mobile_dir / name, output / name)
    for target in TARGETS:
        cdir = input_root / 'native' / target
        for base in (f'vibeface-{pv}-{target}.tar.gz', f'vibeface-{pv}-{target}.tar.gz.sha256',
                     f'vibeface-core-{pv}-{target}.tar.gz', f'vibeface-core-{pv}-{target}.tar.gz.sha256'):
            shutil.copy2(cdir / base, output / base)
    manifest_release('product', output, source_commit=src_commit)
    final_files = {p.name for p in output.iterdir() if p.is_file()}
    require(final_files == _expected_product_assets(pv), 'assembled product file set incomplete')


def plan(release_app, tag=None, require_main=False, github_output=None, workflow_dispatch_check=True):
    require(release_app in RELEASES, 'plan --app product|www')
    if tag:
        rk, ver, ch = parse_tag(tag)
        require(rk == release_app, 'tag kind must match --app')
        if release_app == 'product':
            require(ver == canonical_product_version(), 'tag version must match declared product versions')
        else:
            require(ver == read_version('www'), 'tag version must match www package.json')
        data = identity_release(release_app)
        require(data['tag'] == tag, 'tag does not match declared version')
    else:
        data = identity_release(release_app)
    if workflow_dispatch_check and os.environ.get('GITHUB_EVENT_NAME') == 'workflow_dispatch':
        require(os.environ.get('GITHUB_REF') == 'refs/tags/' + data['tag'], 'dispatch ref must match release tag')
    if require_main:
        subprocess.run(['git', 'merge-base', '--is-ancestor', 'HEAD', 'origin/main'], check=True)
        require(_git_head_clean(), 'release source must be clean')
    if github_output:
        with open(github_output, 'a') as f:
            for key, value in data.items():
                f.write(f'{key}={value}\n')
    print(json.dumps(data))


def self_test():
    from unittest.mock import patch
    pv = canonical_product_version()
    ch = version(pv)
    src = 'a' * 40
    tag_p = f'v{pv}'

    assert parse_tag(tag_p) == ('product', pv, ch)
    assert parse_tag(f'www-v{pv}') == ('www', pv, ch)
    for bad in ('v01.0.1', 'v1', 'www-v', 'app-v0.0.1', 'tag-v0.0.1', 'www-v0.0.1-rc'):
        try:
            parse_tag(bad)
        except ValueError:
            pass
        else:
            raise AssertionError(f'accepted invalid tag {bad}')

    scratch = ROOT / '.artifacts/release-tests'
    scratch.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=scratch) as tmp:
        tmp = Path(tmp)
        directory = tmp / 'component'
        directory.mkdir()
        (directory / 'web').mkdir()
        (directory / 'web' / 'test').write_text('known content')
        public_archive(directory, tmp / 'asset.tar.gz')
        extracted = unpack(tmp / 'asset.tar.gz', tmp / 'out')
        assert (extracted / 'web' / 'test').read_text() == 'known content'
        info = dict(app='app', schema=SCHEMA_VERSION, tag=tag_p, version=pv,
                    source_commit=src, channel=ch, target=None, files=files(extracted))
        write_json(extracted / 'COMPONENT-INFO.json', info)
        assert verify_component(extracted, 'app', {'source_commit': src}) == info
        for change in ({'schema': 0}, {'schema': 2}, {'channel': 'stable'}):
            write_json(extracted / 'COMPONENT-INFO.json', info | change)
            try:
                verify_component(extracted, 'app')
            except ValueError:
                pass
            else:
                raise AssertionError(f'accepted invalid component fields {change}')
        write_json(extracted / 'COMPONENT-INFO.json', info)

        public = tmp / 'public'
        public.mkdir()
        public_archive(extracted, public / f'vibeface-app-{pv}.tar.gz')
        exp_rel = dict(app='product', version=pv, tag=tag_p, source_commit=src, channel=ch)
        with patch(f'{__name__}.identity_release', return_value=dict(exp_rel)):
            manifest_release('product', public, source_commit=src)

        apk = tmp / 'fixture.apk'
        apk.write_bytes(b'APK fixture')
        expo = json.loads((ROOT / 'apps/mobile/app.json').read_text())['expo']
        badging = f"package: name='{expo['android']['package']}' versionCode='{expo['android']['versionCode']}' versionName='{expo['version']}'"

        def run_mock_release(*args, **kwargs):
            if args[0] == 'git':
                sub = args[1]
                if sub == 'rev-parse' and 'HEAD' in args:
                    return src
                if sub == 'status':
                    return ''
                raise AssertionError(args)
            if args[-1] == '--version':
                p = str(args[0])
                return f'vibeface {pv}' if p.endswith('vibeface') and 'vibeface-core' not in p else f'vibeface-core {pv}'
            if len(args) >= 3 and args[1:3] == ('dump', 'badging'):
                return badging
            raise AssertionError(args)

        root_in = tmp / 'assemble-in'
        (root_in / 'app').mkdir(parents=True)
        (root_in / 'mobile').mkdir(parents=True)
        (root_in / 'native').mkdir(parents=True)
        shutil.copy2(public / f'vibeface-app-{pv}.tar.gz', root_in / 'app' / f'vibeface-app-{pv}.tar.gz')
        (root_in / 'app' / f'vibeface-app-{pv}.tar.gz.sha256').write_text(
            f'{digest(root_in / "app" / f"vibeface-app-{pv}.tar.gz")}  vibeface-app-{pv}.tar.gz\n')

        app_prod_id = dict(app='product', version=pv, tag=tag_p, source_commit=src, channel=ch)
        with patch(f'{__name__}.identity_release', return_value=app_prod_id):
            manifest_release('product', root_in / 'app', source_commit=src)

        with patch(f'{__name__}.shutil.which', return_value='aapt'), patch(f'{__name__}.run', side_effect=run_mock_release):
            shutil.copy2(apk, root_in / 'mobile' / f'vibeface-{pv}-android.apk')
            prov = mobile_provenance(root_in / 'mobile' / f'vibeface-{pv}-android.apk')
            write_json(root_in / 'mobile' / f'vibeface-{pv}-android.apk.provenance.json', prov)
            (root_in / 'mobile' / f'vibeface-{pv}-android.apk.sha256').write_text(
                f'{digest(root_in / "mobile" / f"vibeface-{pv}-android.apk")}  vibeface-{pv}-android.apk\n')
            assert prov['sha256'] == digest(root_in / 'mobile' / f'vibeface-{pv}-android.apk')
            mob_id = dict(app='mobile', version=pv, tag=tag_p, source_commit=src, channel=ch)
            with patch(f'{__name__}.identity', return_value=mob_id):
                manifest('mobile', root_in / 'mobile')
            for changed_bad in (badging.replace(expo['android']['package'], 'wrong.package'), 'bad APK'):
                def bad_run(*a, **k):
                    return changed_bad if len(a) >= 3 and a[1:3] == ('dump', 'badging') else run_mock_release(*a, **k)
                with patch(f'{__name__}.run', side_effect=bad_run):
                    try:
                        mobile_provenance(apk)
                    except ValueError:
                        pass
                    else:
                        raise AssertionError('accepted incorrect APK identity')

        native_bin = tmp / 'native-bin'
        native_bin.mkdir()
        (native_bin / 'vibeface').write_text('#!/bin/sh\necho "vibeface ' + pv + '"\n')
        (native_bin / 'vibeface-core').write_text('#!/bin/sh\necho "vibeface-core ' + pv + '"\n')
        (native_bin / 'vibeface').chmod(0o755)
        (native_bin / 'vibeface-core').chmod(0o755)
        with patch(f'{__name__}.run', side_effect=run_mock_release):
            for tgt in TARGETS:
                stage_dir = tmp / f'stage-{tgt}'
                stage_native(tgt, native_bin, stage_dir)
                compose_offline(tgt, stage_dir, root_in / 'app' / f'vibeface-app-{pv}.tar.gz', root_in / 'native' / tgt)

            assemble_out = tmp / 'assembled'
            assemble_product(root_in, assemble_out)

        product_public = tmp / 'public-product'
        shutil.copytree(assemble_out, product_public)

        fake_product_files = {
            f'vibeface-app-{pv}.tar.gz', f'vibeface-app-{pv}.tar.gz.sha256',
            f'vibeface-{pv}-android.apk', f'vibeface-{pv}-android.apk.sha256', f'vibeface-{pv}-android.apk.provenance.json',
            'release.json',
        }
        for t in TARGETS:
            fake_product_files.add(f'vibeface-{pv}-{t}.tar.gz')
            fake_product_files.add(f'vibeface-{pv}-{t}.tar.gz.sha256')
            fake_product_files.add(f'vibeface-core-{pv}-{t}.tar.gz')
            fake_product_files.add(f'vibeface-core-{pv}-{t}.tar.gz.sha256')

        def product_public_query(*args, **kwargs):
            if args[:3] == ('gh', 'release', 'view'):
                return json.dumps(dict(isDraft=False, assets=[{'name': n} for n in sorted(fake_product_files)]))
            if args[:2] == ('gh', 'api'):
                return json.dumps(dict(sha=src))
            if args[0] == 'git' and args[1] == 'rev-parse' and 'HEAD' in args:
                return src
            if len(args) >= 3 and args[1:3] == ('dump', 'badging'):
                return badging
            raise AssertionError(args)

        def download_product(*args, **kwargs):
            command = args[0]
            assert command[:3] == ['gh', 'release', 'download']
            destination = Path(command[command.index('--dir') + 1])
            for path in product_public.iterdir():
                shutil.copy2(path, destination / path.name)
            return subprocess.CompletedProcess(command, 0)

        exp_full = dict(exp_rel, schema=SCHEMA_VERSION)
        exp_full['assets'] = json.loads((product_public / 'release.json').read_text())['assets']
        with patch(f'{__name__}.identity_release', return_value=dict(exp_rel)), \
                patch(f'{__name__}.run', side_effect=product_public_query), \
                patch(f'{__name__}.subprocess.run', side_effect=download_product):
            assert verify_public('product', tmp / 'dl-product') == exp_full

        changed = json.loads((product_public / 'release.json').read_text())
        changed['assets'][0]['sha256'] = '0' * 64
        write_json(product_public / 'release.json', changed)
        try:
            with patch(f'{__name__}.identity_release', return_value=dict(exp_rel)), \
                    patch(f'{__name__}.run', side_effect=product_public_query), \
                    patch(f'{__name__}.subprocess.run', side_effect=download_product):
                verify_public('product', tmp / 'dl-bad')
        except ValueError as error:
            assert 'checksum' in str(error)
        else:
            raise AssertionError('accepted corrupt public manifest')

        build_base = dict(version=pv, target=TARGETS[0], source_commit=src,
                          update_protocol=control_constant('UPDATE_PROTOCOL'), data_epoch=control_constant('DATA_EPOCH'),
                          core_version=pv, core_source_commit=src, app_version=pv, app_source_commit=src,
                          component_source_commits={'core': src, 'app': src})
        verify_build_info(build_base)
        for change in ({'data_epoch': 12}, {'component_source_commits': {'core': src}}):
            try:
                verify_build_info(build_base | change)
            except ValueError:
                pass
            else:
                raise AssertionError('accepted invalid build info')

        partial = tmp / 'public-partial'
        shutil.copytree(product_public, partial)
        for path in list(partial.iterdir()):
            if path.name not in ('release.json', f'vibeface-app-{pv}.tar.gz', f'vibeface-app-{pv}.tar.gz.sha256'):
                path.unlink()

        def download_partial(*args, **kwargs):
            command = args[0]
            destination = Path(command[command.index('--dir') + 1])
            for path in partial.iterdir():
                shutil.copy2(path, destination / path.name)
            return subprocess.CompletedProcess(command, 0)

        try:
            with patch(f'{__name__}.identity_release', return_value=dict(exp_rel)), \
                    patch(f'{__name__}.run', side_effect=product_public_query), \
                    patch(f'{__name__}.subprocess.run', side_effect=download_partial):
                verify_public('product', tmp / 'dl-partial')
        except ValueError as error:
            assert 'missing regular release asset' in str(error) or 'checksum' in str(error)
        else:
            raise AssertionError('accepted incomplete product release set')

    print('release identities and archive self-tests passed')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='command', required=True)
    p = sub.add_parser('plan')
    p.add_argument('--app', choices=RELEASES, required=True)
    p.add_argument('--tag')
    p.add_argument('--github-output')
    p.add_argument('--require-main', action='store_true')
    p = sub.add_parser('component')
    p.add_argument('app', choices=('core', 'app', 'www'))
    p.add_argument('--input', type=Path, required=True)
    p.add_argument('--output', type=Path, required=True)
    p.add_argument('--target')
    p = sub.add_parser('stage-native')
    p.add_argument('--target', required=True)
    p.add_argument('--input', type=Path, required=True)
    p.add_argument('--output', type=Path, required=True)
    p = sub.add_parser('compose')
    p.add_argument('--target', required=True)
    p.add_argument('--native-dir', type=Path, required=True)
    p.add_argument('--app-archive', type=Path, required=True)
    p.add_argument('--output', type=Path, required=True)
    sub.add_parser('seal-bundle').add_argument('directory', type=Path)
    p = sub.add_parser('manifest')
    p.add_argument('app', choices=RELEASES + ('mobile',))
    p.add_argument('directory', type=Path)
    ap = sub.add_parser('assemble-product')
    ap.add_argument('--input-root', type=Path, required=True)
    ap.add_argument('--output', type=Path, required=True)
    p = sub.add_parser('publish')
    p.add_argument('app', choices=RELEASES + ('mobile',))
    p.add_argument('directory', type=Path)
    p = sub.add_parser('verify-public')
    p.add_argument('app', choices=RELEASES + ('mobile',))
    p.add_argument('--output', required=True, type=Path)
    p.add_argument('--repo', default=GITHUB_REPO)
    p = sub.add_parser('mobile-provenance')
    p.add_argument('apk', type=Path)
    sub.add_parser('self-test')
    args = parser.parse_args()
    if args.command == 'plan':
        plan(args.app, tag=args.tag, require_main=args.require_main, github_output=args.github_output)
    elif args.command == 'component':
        pack_component(args.app, args.input, args.output, args.target)
    elif args.command == 'stage-native':
        stage_native(args.target, args.input, args.output)
    elif args.command == 'compose':
        compose_offline(args.target, args.native_dir, args.app_archive, args.output)
    elif args.command == 'seal-bundle':
        seal_bundle(args.directory)
    elif args.command == 'manifest':
        manifest(args.app, args.directory)
    elif args.command == 'assemble-product':
        assemble_product(args.input_root, args.output)
    elif args.command == 'publish':
        if args.app in RELEASES:
            publish(args.app, args.directory)
        else:
            publish_mobile(args.directory)
    elif args.command == 'verify-public':
        print(json.dumps(verify_public(args.app, args.output, repo=args.repo)))
    elif args.command == 'mobile-provenance':
        write_json(args.apk.with_name(args.apk.name + '.provenance.json'), mobile_provenance(args.apk))
    else:
        self_test()


if __name__ == '__main__':
    main()
