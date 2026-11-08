#!/usr/bin/env python3
"""Product release identity, immutable component archives and offline composition."""
import argparse
import contextlib
import datetime
import hashlib
import gzip
import io
import json
import os
from pathlib import Path, PurePosixPath, PureWindowsPath
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import plistlib
import zipfile

sys.dont_write_bytecode = True

ROOT = Path(__file__).resolve().parent.parent
APPS = ('www', 'app', 'core', 'cli', 'mobile', 'desktop')
PRODUCT_APPS = ('app', 'core', 'cli', 'mobile', 'desktop')
RELEASES = ('product', 'www')
COMPONENTS = ('app', 'core', 'www')
TARGETS = ('aarch64-apple-darwin', 'aarch64-unknown-linux-gnu', 'x86_64-unknown-linux-gnu',
           'x86_64-pc-windows-msvc')
WINDOWS_TARGETS = ('x86_64-pc-windows-msvc',)
DESKTOP_TARGETS = TARGETS
DESKTOP_INSTALLERS = {'aarch64-apple-darwin': 'dmg', 'x86_64-pc-windows-msvc': 'exe',
                      'x86_64-unknown-linux-gnu': 'AppImage', 'aarch64-unknown-linux-gnu': 'AppImage'}
ELECTRON_VERSION = '45.2.3'
SIGNING_STATES = ('developer-id-notarized', 'authenticode', 'unsigned-development-preview', 'platform-checksums')
VERSION = r'(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-(nightly)\.([0-9]{8})\.([1-9][0-9]*)|-(rc)\.([1-9][0-9]*))?'
COMMIT = r'[a-f0-9]{40}'
MAX_ARCHIVE = 384 * 1024 * 1024
MAX_UNPACKED = 512 * 1024 * 1024
SCHEMA_VERSION = 1
GITHUB_REPO = 'leaperone/vibeface'


def require(condition, message):
    if not condition:
        raise ValueError(message)


def run(*args, cwd=None):
    return subprocess.check_output(args, cwd=cwd or ROOT, text=True).strip()


def program_suffix(target):
    return '.exe' if target in WINDOWS_TARGETS else ''


def program_names(target):
    return tuple(name + program_suffix(target) for name in ('vibeface', 'vibeface-core'))


def service_installer(target):
    """The user service installer shipped inside a bundle for this platform."""
    return ROOT / 'scripts' / ('install-node-service.ps1' if target in WINDOWS_TARGETS
                               else 'install-node-service.sh')


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
        text = (directory / 'Cargo.toml').read_text(encoding='utf-8')
        match = re.search(r'^version\s*=\s*"([^"]+)"', text, re.M)
        require(match, f'{app} must own its package version')
        return match[1]
    data = json.loads((directory / ('app.json' if app == 'mobile' else 'package.json')).read_text(encoding='utf-8'))
    return data['expo']['extra']['releaseVersion'] if app == 'mobile' else data['version']


def canonical_product_version(root=None):
    return read_version('cli', root)


def require_version_cohort(apps, root=None):
    """Every named app shares the canonical CLI version; the product has exactly one version."""
    value = canonical_product_version(root)
    for app in apps:
        require(read_version(app, root) == value, f'{app} version must equal the CLI product version')
    return value


def release_tag_for(release_app, version_value):
    require(release_app in RELEASES, 'unknown release app')
    return f'v{version_value}' if release_app == 'product' else f'www-v{version_value}'


def identity(release_app, root=None):
    """Canonical public release identity: the whole product, or the independent portal."""
    root = root or ROOT
    require(release_app in RELEASES, 'release app must be product or www')
    if release_app == 'product':
        value = require_version_cohort(PRODUCT_APPS, root)
    else:
        value = read_version('www', root)
    return {'app': release_app, 'version': value, 'tag': release_tag_for(release_app, value),
            'source_commit': run('git', 'rev-parse', 'HEAD', cwd=root), 'channel': version(value)}


def expected_release_kind_for_component(app):
    return 'www' if app == 'www' else 'product'


def component_identity(app, root=None):
    """Internal component identity; product components carry the product version and tag."""
    root = root or ROOT
    require(app in COMPONENTS, 'unknown component')
    release = identity(expected_release_kind_for_component(app), root)
    require(read_version(app, root) == release['version'], 'component version must equal its release version')
    return dict(release, app=app)


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
    for path in sorted(directory.rglob('*'), key=lambda path: path.relative_to(directory).as_posix()):
        require(not path.is_symlink(), 'symlinks are not allowed in release assets')
        if path.is_file():
            name = path.relative_to(directory).as_posix()
            safe_name(name)
            if name not in exclude:
                result[name] = digest(path)
    require(len(result) <= 4096, 'too many release files')
    return result


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + '\n', encoding='utf-8', newline='\n')


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
    output.with_name(output.name + '.sha256').write_text(f'{digest(output)}  {output.name}\n', encoding='utf-8', newline='\n')


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


def control_constant(name):
    match = re.search(rf'pub const {name}: u32 = (\d+);', (ROOT / 'crates/control/src/lib.rs').read_text(encoding='utf-8'))
    require(match, f'missing {name}')
    return int(match[1])


def verify_component(directory, app, expected=None):
    info = json.loads((directory / 'COMPONENT-INFO.json').read_text(encoding='utf-8'))
    fields = {'app', 'version', 'tag', 'source_commit', 'channel', 'schema', 'target', 'files'}
    if app == 'core':
        fields.add('data_epoch')
    require(set(info) == fields, 'unsupported component fields')
    require(info['app'] == app and type(info['schema']) is int and info['schema'] == SCHEMA_VERSION, 'wrong component identity')
    rk, v, ch = parse_tag(info['tag'])
    require(rk == expected_release_kind_for_component(app) and v == info['version'] and ch == info['channel'], 'invalid component tag')
    require(re.fullmatch(COMMIT, info['source_commit']), 'invalid component source')
    if app == 'core':
        require(type(info['data_epoch']) is int and info['data_epoch'] == control_constant('DATA_EPOCH') and info['target'] in TARGETS,
                'unsupported Core data epoch or target')
    else:
        require(info['target'] is None, 'static component must not specify a native target')
    if app in ('app', 'www'):
        programs = {name for target in TARGETS for name in program_names(target)}
        require(not any(p.name in programs for p in directory.rglob('*')), 'static component must not include native programs')
    if expected:
        require(all(info[k] == v for k, v in expected.items()), 'component pin mismatch')
    require(info['files'] == files(directory, ('COMPONENT-INFO.json',)), 'component content changed')
    return info


def pack_component(app, source, output, target=None):
    info = component_identity(app)
    require(not run('git', 'status', '--porcelain'), 'release source must be clean')
    if app == 'core':
        require(target in TARGETS and source.is_file(), 'core requires a target binary')
        require(run(str(source.resolve()), '--version') == 'vibeface-core ' + info['version'], 'core binary version mismatch')
        require(source.name == program_names(target)[1], 'core binary name must match its platform')
    else:
        require(app in ('app', 'www') and source.is_dir(), 'expected static app directory')
    name = f'vibeface-{app}-{info["version"]}' + (f'-{target}' if target else '')
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=output, prefix='.component-') as tmp:
        directory = Path(tmp) / name; directory.mkdir()
        if app == 'core':
            shutil.copy2(source, directory / program_names(target)[1])
        else:
            shutil.copytree(source, directory / ('web' if app == 'app' else 'site'))
        info.update(schema=SCHEMA_VERSION, target=target, files=files(directory))
        if app == 'core':
            info['data_epoch'] = control_constant('DATA_EPOCH')
        write_json(directory / 'COMPONENT-INFO.json', info)
        verify_component(directory, app)
        public_archive(directory, output / f'{name}.tar.gz')
    manifest(expected_release_kind_for_component(app), output)


def verify_build_info(info):
    require(set(info) == {'version', 'target', 'source_commit', 'update_protocol', 'data_epoch',
                          'core_version', 'core_source_commit', 'app_version', 'app_source_commit'},
            'unsupported build info fields')
    require(info['target'] in TARGETS, 'unsupported build target')
    for key, constant in (('update_protocol', 'UPDATE_PROTOCOL'), ('data_epoch', 'DATA_EPOCH')):
        require(type(info[key]) is int and info[key] == control_constant(constant), 'unsupported build protocol or data epoch')
    for key in ('source_commit', 'core_source_commit', 'app_source_commit'):
        require(isinstance(info[key], str) and re.fullmatch(COMMIT, info[key]), 'invalid build source')
    version(info['version'])
    require(info['core_version'] == info['version'] == info['app_version'],
            'CLI, Core and App must carry one product version')
    require(info['core_source_commit'] == info['source_commit'] == info['app_source_commit'],
            'CLI, Core and App must carry one source commit')


def seal_bundle(directory):
    """Seal a composed bundle: CLI, Core and App share this checkout's version and commit."""
    product_version = require_version_cohort(('core', 'app'))
    require(not run('git', 'status', '--porcelain'), 'package source must be clean')
    source = run('git', 'rev-parse', 'HEAD')
    build = dict(version=product_version, target=os.environ['PACKAGE_TARGET'], source_commit=source,
                 update_protocol=control_constant('UPDATE_PROTOCOL'), data_epoch=control_constant('DATA_EPOCH'),
                 core_version=product_version, core_source_commit=source,
                 app_version=product_version, app_source_commit=source)
    verify_build_info(build)
    write_json(directory / 'BUILD-INFO.json', build)
    content = files(directory, ('FILES.sha256',))
    (directory / 'FILES.sha256').write_text(''.join(f'{value}  {name}\n' for name, value in content.items()), encoding='utf-8', newline='\n')
    return build


def manifest(release_app, output):
    """Release manifest for a complete or staged partial asset set of one release."""
    require(release_app in RELEASES, 'manifest requires the product or www release app')
    info = identity(release_app)
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
    require(info['app'] in RELEASES, 'release manifest app must be product or www')
    rk, v, ch = parse_tag(info['tag'])
    require(rk == info['app'] and v == info['version'] and ch == info['channel'], 'invalid release identity')
    require(re.fullmatch(COMMIT, info['source_commit']), 'invalid release commit')


def verify_manifest(directory, expected=None):
    info = json.loads((directory / 'release.json').read_text(encoding='utf-8'))
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


def native_receipt(native_dir, target, product_version, source_commit):
    """Read and bind a native staging receipt to this target, version and checkout."""
    path = native_dir / 'NATIVE-INFO.json'
    require(path.is_file(), 'missing NATIVE-INFO.json')
    info = json.loads(path.read_text(encoding='utf-8'))
    require(set(info) == {'schema', 'target', 'version', 'source_commit', 'files', 'data_epoch'},
            'unsupported native receipt fields')
    for key, value in (('schema', SCHEMA_VERSION), ('data_epoch', control_constant('DATA_EPOCH'))):
        require(type(info[key]) is int and info[key] == value, 'invalid native receipt schema or data epoch')
    require(info['target'] == target, 'native receipt target mismatch')
    require(info['version'] == product_version, 'native receipt version mismatch')
    require(isinstance(info['source_commit'], str) and re.fullmatch(COMMIT, info['source_commit']), 'invalid native source commit')
    require(info['source_commit'] == source_commit, 'native staging source must match this checkout')
    require(set(info['files']) == set(program_names(target)), 'native receipt must list vibeface and vibeface-core')
    require(files(native_dir, ('NATIVE-INFO.json',)) == info['files'], 'native staging content changed or has extra files')
    return info


def stage_native(target, binary_directory, output):
    require(target in TARGETS, 'unsupported platform')
    binary_directory = binary_directory.resolve()
    require(binary_directory.is_dir(), 'native input must be a directory')
    binaries = [binary_directory / name for name in program_names(target)]
    for binary in binaries:
        require(binary.is_file() and not binary.is_symlink(), 'native directory must contain vibeface and vibeface-core')
    product_version = identity('product')['version']
    require(run(str(binaries[0]), '--version') == f'vibeface {product_version}', 'CLI binary version mismatch')
    require(run(str(binaries[1]), '--version') == f'vibeface-core {product_version}', 'core binary version mismatch')
    require(not run('git', 'status', '--porcelain'), 'release source must be clean')
    source_commit = run('git', 'rev-parse', 'HEAD')
    output.mkdir(parents=True, exist_ok=True)
    require(not any(output.iterdir()), 'native staging output must be empty')
    for binary in binaries:
        shutil.copy2(binary, output / binary.name)
    write_json(output / 'NATIVE-INFO.json', dict(
        schema=SCHEMA_VERSION, target=target, version=product_version, source_commit=source_commit,
        files=files(output, ('NATIVE-INFO.json',)), data_epoch=control_constant('DATA_EPOCH')))
    return native_receipt(output, target, product_version, source_commit)


def compose_offline(target, native_dir, app_archive, output):
    """Offline CLI bundle and standalone Core archive from one checkout's native and App inputs."""
    require(target in TARGETS, 'unsupported platform')
    native_dir = native_dir.resolve()
    app_archive = app_archive.resolve()
    product = identity('product')
    require(not run('git', 'status', '--porcelain'), 'release source must be clean')
    product_version = product['version']
    source_commit = product['source_commit']
    receipt = native_receipt(native_dir, target, product_version, source_commit)
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=output, prefix='.compose-') as tmp:
        tmp = Path(tmp)
        unpacked_app = unpack(app_archive, tmp / 'app-unpack')
        verify_component(unpacked_app, 'app', dict(version=product_version, tag=product['tag'], target=None,
                                                   source_commit=source_commit, channel=product['channel']))
        cli_name, core_name = program_names(target)
        cli_src = native_dir / cli_name
        core_src = native_dir / core_name
        require(digest(cli_src) == receipt['files'][cli_name] and digest(core_src) == receipt['files'][core_name],
                'native binaries do not match the staging receipt')
        directory = tmp / f'vibeface-{product_version}-{target}'
        directory.mkdir()
        shutil.copy2(cli_src, directory / cli_name)
        shutil.copy2(core_src, directory / core_name)
        shutil.copytree(unpacked_app / 'web', directory / 'web')
        (directory / 'scripts').mkdir()
        installer = service_installer(target)
        require(installer.is_file(), f'missing user service installer for {target}: scripts/{installer.name}')
        shutil.copy2(installer, directory / 'scripts' / installer.name)
        shutil.copy2(ROOT / 'README.md', directory / 'README.md')
        (directory / 'docs').mkdir()
        for name in ('node.md', 'releasing.md'):
            shutil.copy2(ROOT / 'packages/docs' / name, directory / 'docs' / name)
        (directory / 'RUNTIME-REQUIREMENTS.md').write_text('Requires separately configured Agent programs and credentials.\n')
        os.environ['PACKAGE_TARGET'] = target
        seal_bundle(directory)
        bundle = tmp / f'vibeface-{product_version}-{target}.tar.gz'
        public_archive(directory, bundle)
        core_only = tmp / f'vibeface-core-{product_version}-{target}'
        core_only.mkdir()
        shutil.copy2(core_src, core_only / core_name)
        core_info = dict(component_identity('core'), schema=SCHEMA_VERSION, target=target,
                         data_epoch=control_constant('DATA_EPOCH'), files=files(core_only))
        write_json(core_only / 'COMPONENT-INFO.json', core_info)
        verify_component(core_only, 'core', dict(version=product_version, tag=product['tag'],
                                                 source_commit=source_commit, target=target))
        core_archive = tmp / f'vibeface-core-{product_version}-{target}.tar.gz'
        public_archive(core_only, core_archive)
        for path in (bundle, bundle.with_name(bundle.name + '.sha256'),
                     core_archive, core_archive.with_name(core_archive.name + '.sha256')):
            shutil.copy2(path, output / path.name)
    manifest('product', output)


def mobile_provenance(apk):
    expo = json.loads((ROOT / 'apps/mobile/app.json').read_text(encoding='utf-8'))['expo']
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


def desktop_basename(version_value, target):
    return f'vibeface-desktop-{version_value}-{target}'


def desktop_payload_names(version_value, target):
    base = desktop_basename(version_value, target)
    return (f'{base}.{DESKTOP_INSTALLERS[target]}', f'{base}.zip')


def desktop_asset_names(version_value, target):
    names = {f'{desktop_basename(version_value, target)}.provenance.json'}
    for name in desktop_payload_names(version_value, target):
        names |= {name, name + '.sha256'}
    return names


def verify_desktop_receipt(directory, target, pins, runtime_digest=None):
    """Read a desktop provenance receipt and bind it to its payloads, cohort and bundled runtime."""
    require(target in DESKTOP_TARGETS, 'unsupported desktop target')
    version_value = pins['version']
    path = directory / f'{desktop_basename(version_value, target)}.provenance.json'
    require(path.is_file(), 'missing desktop provenance receipt')
    info = json.loads(path.read_text(encoding='utf-8'))
    require(set(info) == {'schema', 'app', 'target', 'version', 'tag', 'channel', 'source_commit',
                          'electron', 'signed', 'signing', 'runtime', 'artifacts'},
            'unsupported desktop receipt fields')
    require(type(info['schema']) is int and info['schema'] == SCHEMA_VERSION, 'invalid desktop receipt schema')
    require(info['app'] == 'desktop' and info['target'] == target, 'desktop receipt is not this platform')
    require(all(info[key] == value for key, value in pins.items()), 'desktop receipt cohort mismatch')
    require(re.fullmatch(COMMIT, info['source_commit']), 'invalid desktop source commit')
    require(info['electron'] == ELECTRON_VERSION, 'desktop receipt must name the pinned Electron version')
    require(type(info['signed']) is bool and info['signing'] in SIGNING_STATES, 'invalid desktop signing status')
    require(info['signed'] == (info['signing'] in ('developer-id-notarized', 'authenticode')),
            'desktop signing status and signed flag disagree')
    linux = target.endswith('-unknown-linux-gnu')
    require((info['signing'] == 'platform-checksums') == linux,
            'Linux platform checksums must match the desktop target')
    require(info['signed'] or linux or pins['channel'] != 'stable',
            'stable desktop distribution requires a signed application')
    expected_signing = 'developer-id-notarized' if target == 'aarch64-apple-darwin' else 'authenticode'
    require(not info['signed'] or info['signing'] == expected_signing, 'desktop signing kind does not match its platform')
    require(set(info['runtime']) == {'archive', 'sha256'}, 'unsupported desktop runtime reference')
    require(info['runtime']['archive'] == f'vibeface-{version_value}-{target}.tar.gz',
            'desktop runtime must be this platform CLI bundle')
    require(isinstance(info['runtime']['sha256'], str) and re.fullmatch(r'[a-f0-9]{64}', info['runtime']['sha256']),
            'invalid desktop runtime digest')
    if runtime_digest:
        require(info['runtime']['sha256'] == runtime_digest, 'desktop runtime differs from the released CLI bundle')
    payloads = desktop_payload_names(version_value, target)
    require(set(info['artifacts']) == set(payloads), 'desktop receipt must cover exactly its two payloads')
    for name in payloads:
        payload = directory / name
        require(payload.is_file() and not payload.is_symlink(), 'missing regular desktop payload')
        require(digest(payload) == info['artifacts'][name], 'desktop payload digest mismatch')
        verify_checksum_file(directory, name)
    return info


def apk_asset_names(version_value):
    name = f'vibeface-{version_value}-android.apk'
    return {name, name + '.sha256', name + '.provenance.json'}


def ios_asset_names(version_value, signed=False):
    state = 'signed' if signed else 'unsigned'
    payloads = [f'vibeface-{version_value}-ios-device-{state}.xcarchive.zip']
    if signed:
        payloads.append(f'vibeface-{version_value}-ios.ipa')
    return {name + suffix for name in payloads for suffix in ('', '.sha256', '.provenance.json')}


def verify_ios_assets(directory, pins):
    version_value = pins['version']
    signed = (directory / f'vibeface-{version_value}-ios.ipa').is_file()
    mobile = json.loads((ROOT / 'apps/mobile/app.json').read_text(encoding='utf-8'))['expo']
    identity_fields = dict(schema=1, platform='ios-device', source_sha=pins['source_commit'],
                           release_version=version_value, bundle_identifier=mobile['ios']['bundleIdentifier'],
                           bundle_short_version=mobile['version'], build_number=mobile['ios']['buildNumber'])
    state = 'signed' if signed else 'unsigned'
    payloads = [f'vibeface-{version_value}-ios-device-{state}.xcarchive.zip']
    if signed:
        payloads.append(f'vibeface-{version_value}-ios.ipa')
    for index, name in enumerate(payloads):
        payload = directory / name
        require(payload.is_file() and not payload.is_symlink(), 'missing regular iOS payload')
        verify_checksum_file(directory, name)
        receipt = json.loads((directory / (name + '.provenance.json')).read_text(encoding='utf-8'))
        fields = set(identity_fields) | {'artifact', 'sha256', 'signed', 'installable', 'kind'}
        if index:
            fields |= {'distribution_method', 'team_id', 'provisioning_profile_uuid', 'provisioning_profile_expiration', 'testflight_eligible'}
        require(set(receipt) == fields, 'unsupported iOS receipt fields')
        require(all(receipt.get(key) == value for key, value in identity_fields.items()), 'iOS source, version or app identity mismatch')
        require(receipt.get('artifact') == name and receipt.get('sha256') == digest(payload), 'iOS receipt payload digest mismatch')
        require(receipt.get('signed') is signed, 'iOS signing claim mismatch')
        installable = bool(index) and receipt.get('distribution_method') == 'ad-hoc'
        require(receipt.get('installable') is installable, 'iOS installability claim mismatch')
        if index:
            require(receipt.get('testflight_eligible') is (receipt.get('distribution_method') == 'app-store-connect'),
                    'iOS TestFlight eligibility claim mismatch')
        expected_kind = 'signed-ios-ipa' if index else f'{state}-xcarchive-inspection-only'
        require(receipt.get('kind') == expected_kind, 'iOS receipt must distinguish an archive from an installable IPA')
        if index:
            require(receipt.get('distribution_method') in ('app-store-connect', 'ad-hoc'), 'unsupported iOS distribution method')
            require(isinstance(receipt.get('team_id'), str) and re.fullmatch(r'[A-Z0-9]{10}', receipt['team_id']), 'invalid iOS signing team')
            require(isinstance(receipt.get('provisioning_profile_uuid'), str) and receipt['provisioning_profile_uuid'], 'missing iOS provisioning profile')
        with zipfile.ZipFile(payload) as archive:
            entries = archive.infolist()
            names = [entry.filename for entry in entries]
            require(len(names) == len(set(names)) and len(names) <= 32768, 'iOS archive has duplicate or excessive entries')
            require(all(not PurePosixPath(name).is_absolute() and '..' not in PurePosixPath(name).parts and '\\' not in name for name in names), 'unsafe iOS archive member path')
            info_name = 'Payload/Vibeface.app/Info.plist' if index else 'Vibeface.xcarchive/Products/Applications/Vibeface.app/Info.plist'
            profile_name = info_name.rsplit('/', 1)[0] + '/embedded.mobileprovision'
            require((profile_name in names) is signed, 'iOS archive provisioning state differs from its signing claim')
            info = plistlib.loads(archive.read(info_name))
            require((info.get('CFBundleIdentifier'), info.get('CFBundleShortVersionString'), str(info.get('CFBundleVersion')))
                    == (mobile['ios']['bundleIdentifier'], mobile['version'], str(mobile['ios']['buildNumber'])),
                    'iOS archive app metadata mismatch')
            require(info.get('CFBundleSupportedPlatforms') == ['iPhoneOS'], 'iOS device archive must not contain a simulator app')
    return signed


def www_asset_names(version_value):
    name = f'vibeface-www-{version_value}.tar.gz'
    return {name, name + '.sha256', 'release.json'}


def app_asset_names(version_value):
    name = f'vibeface-app-{version_value}.tar.gz'
    return {name, name + '.sha256'}


def native_asset_names(version_value, target):
    bundle = f'vibeface-{version_value}-{target}.tar.gz'
    core = f'vibeface-core-{version_value}-{target}.tar.gz'
    return {bundle, bundle + '.sha256', core, core + '.sha256'}


def product_asset_names(version_value, ios_signed=False):
    names = app_asset_names(version_value) | apk_asset_names(version_value) | ios_asset_names(version_value, ios_signed) | {'release.json'}
    for target in TARGETS:
        names |= native_asset_names(version_value, target)
    for target in DESKTOP_TARGETS:
        names |= desktop_asset_names(version_value, target)
    return names


def expected_asset_names(release_app, version_value, ios_signed=False):
    return product_asset_names(version_value, ios_signed) if release_app == 'product' else www_asset_names(version_value)


PART_KINDS = ('app', 'native', 'desktop', 'android', 'ios')
BUILDING_MARKER = 'BUILDING — incomplete'


def product_payload_names(version_value, ios_signed=False):
    return product_asset_names(version_value, ios_signed) - {'release.json'}


def part_asset_names(kind, version_value, target=None):
    require(kind in PART_KINDS, 'unknown part kind')
    if kind == 'app':
        require(target is None, 'target is forbidden for app parts')
        return app_asset_names(version_value)
    if kind == 'android':
        require(target is None, 'target is forbidden for android parts')
        return apk_asset_names(version_value)
    if kind == 'ios':
        require(target is None, 'target is forbidden for ios parts')
        return ios_asset_names(version_value, False)
    if kind == 'native':
        require(target in TARGETS, 'native parts require a canonical rust target')
        return native_asset_names(version_value, target)
    require(target in DESKTOP_TARGETS, 'desktop parts require a canonical rust target')
    return desktop_asset_names(version_value, target)


def progressive_part_inventory(version_value, ios_signed=False):
    """Eleven disjoint part groups covering exactly the 44 product payloads."""
    inventory = [('app', None, app_asset_names(version_value))]
    for target in TARGETS:
        inventory.append(('native', target, native_asset_names(version_value, target)))
    for target in DESKTOP_TARGETS:
        inventory.append(('desktop', target, desktop_asset_names(version_value, target)))
    inventory.append(('android', None, apk_asset_names(version_value)))
    inventory.append(('ios', None, ios_asset_names(version_value, ios_signed)))
    seen = {}
    union = set()
    for kind, target, names in inventory:
        require(not (names & union), f'part {kind} overlaps another group')
        union |= names
        seen[(kind, target)] = names
    require(union == product_payload_names(version_value, ios_signed), 'parts must cover every payload')
    return inventory


def product_building_notes(info):
    return (f'{BUILDING_MARKER}; not eligible for installation or update.\n\n'
            f"product {info['version']}; source {info['source_commit']}.")


def product_complete_notes(info):
    return f"product {info['version']}; source {info['source_commit']}."


def require_product_publishing():
    require(not run('git', 'status', '--porcelain'), 'release source must be clean')
    expected = identity('product')
    tag = expected['tag']
    require(run('git', 'rev-parse', tag + '^{commit}') == expected['source_commit'],
            'publishing requires the existing release tag at this source commit')
    subprocess.run(['git', 'merge-base', '--is-ancestor', expected['source_commit'], 'origin/main'], check=True)
    return expected


def release_view(tag, repo=GITHUB_REPO):
    view = subprocess.run(['gh', 'release', 'view', tag, f'--repo={repo}', '--json', 'isDraft,assets,body'],
                          capture_output=True, text=True)
    if view.returncode:
        require('release not found' in view.stderr.lower(), 'cannot determine existing release state')
        return None
    return json.loads(view.stdout)


def list_release_assets(tag, repo=GITHUB_REPO):
    payload = json.loads(run('gh', 'api', f'repos/{repo}/releases/tags/{tag}'))
    return payload['assets']


def remote_asset_sha256(asset):
    field = asset.get('digest')
    if isinstance(field, str) and field.startswith('sha256:'):
        return field[7:]
    return None


def confirm_remote_asset(tag, repo, asset, expected_sha, expected_size, scratch):
    name = asset['name']
    size = asset.get('size')
    if size is not None:
        require(int(size) == expected_size, 'existing published size differs')
    remote_sha = remote_asset_sha256(asset)
    if remote_sha:
        require(remote_sha == expected_sha, 'existing published bytes differ')
        return
    subprocess.run(['gh', 'release', 'download', tag, f'--repo={repo}', '--pattern', name, '--dir', str(scratch)],
                   check=True)
    require(digest(scratch / name) == expected_sha, 'existing published bytes differ')


def verify_part_directory(kind, directory, target=None):
    """Validate one producer directory before a progressive part upload."""
    directory = directory.resolve()
    expected = identity('product')
    product_version = expected['version']
    pins = {key: expected[key] for key in ('app', 'version', 'tag', 'source_commit', 'channel')}
    if kind in ('native', 'desktop'):
        require(target, 'native and desktop parts require --target')
        require(target in TARGETS if kind == 'native' else target in DESKTOP_TARGETS, 'unsupported part target')
    else:
        require(target is None, 'target is forbidden for this part kind')
    ios_signed = False
    if kind == 'ios':
        ios_signed = verify_ios_assets(directory, pins)
        payload = ios_asset_names(product_version, ios_signed)
    else:
        payload = part_asset_names(kind, product_version, target)
    producer_manifest(directory, pins)
    require({p.name for p in directory.iterdir()} == payload | {'release.json'},
            'part file set is incomplete or carries extra files')
    for name in sorted(payload):
        if (directory / (name + '.sha256')).is_file():
            verify_checksum_file(directory, name)
    if kind == 'android':
        apk = directory / f'vibeface-{product_version}-android.apk'
        provenance = json.loads((directory / (apk.name + '.provenance.json')).read_text(encoding='utf-8'))
        require(provenance == mobile_provenance(apk), 'APK provenance mismatch')
        require(provenance['version'] == product_version and provenance['sourceCommit'] == pins['source_commit'],
                'APK is not part of this product cohort')
    elif kind == 'desktop':
        verify_desktop_receipt(directory, target, pins)
    return dict(pins, kind=kind, target=target, names=frozenset(payload), ios_signed=ios_signed)


def append_only_assets(tag, repo, upload_map, remote_by_name, scratch):
    """Append absent payloads only; equal bytes are a no-op; conflicts fail closed."""
    for name, path in upload_map.items():
        require(name != 'release.json', 'producer release.json must never be uploaded')
        size = path.stat().st_size
        expected_sha = digest(path)
        if name in remote_by_name:
            confirm_remote_asset(tag, repo, remote_by_name[name], expected_sha, size, scratch)
            continue
        result = subprocess.run(['gh', 'release', 'upload', tag, f'--repo={repo}', str(path)],
                                capture_output=True, text=True)
        if result.returncode:
            refreshed = {asset['name']: asset for asset in list_release_assets(tag, repo)}
            if name in refreshed:
                confirm_remote_asset(tag, repo, refreshed[name], expected_sha, size, scratch)
                continue
            require(False, 'cannot determine remote upload outcome')
        matches = [asset for asset in list_release_assets(tag, repo) if asset['name'] == name]
        require(len(matches) == 1, 'uploaded asset missing after write')
        confirm_remote_asset(tag, repo, matches[0], expected_sha, size, scratch)


def begin_product(repo=GITHUB_REPO):
    """Create or resume one public product Release in BUILDING state."""
    expected = require_product_publishing()
    tag = expected['tag']
    notes = product_building_notes(expected)
    prerelease = str(expected['channel'] != 'stable').lower()
    view = release_view(tag, repo)
    if view is None:
        subprocess.run(['gh', 'release', 'create', tag, f'--repo={repo}', '--verify-tag', '--draft=false',
                        '--title', tag, '--notes', notes, '--latest=false', f'--prerelease={prerelease}'], check=True)
        return expected
    source = json.loads(run('gh', 'api', f'repos/{repo}/commits/{tag}'))['sha']
    require(source == expected['source_commit'], 'public tag source mismatch')
    remote = {asset['name'] for asset in list_release_assets(tag, repo)}
    ios_signed = f'vibeface-{expected["version"]}-ios.ipa' in remote
    if 'release.json' in remote:
        require(remote == product_asset_names(expected['version'], ios_signed),
                'immutable complete release cannot change')
    if BUILDING_MARKER not in (view.get('body') or ''):
        subprocess.run(['gh', 'release', 'edit', tag, f'--repo={repo}', '--notes', notes], check=True)
    return expected


def publish_part(kind, directory, target=None, repo=GITHUB_REPO):
    """Validate and append one progressive product part without touching the final manifest."""
    part = verify_part_directory(kind, directory, target)
    expected = require_product_publishing()
    require(all(part[key] == expected[key] for key in ('version', 'tag', 'source_commit', 'channel')),
            'part identity mismatch')
    tag = expected['tag']
    view = release_view(tag, repo)
    require(view is not None, 'release must be opened with begin-product first')
    require(not view['isDraft'], 'progressive parts require a public building release')
    remote = list_release_assets(tag, repo)
    remote_by_name = {asset['name']: asset for asset in remote}
    require('release.json' not in remote_by_name, 'cannot publish parts after release manifest is public')
    upload_map = {name: directory / name for name in sorted(part['names'])}
    with tempfile.TemporaryDirectory(prefix='vibeface-part-') as temporary:
        append_only_assets(tag, repo, upload_map, remote_by_name, Path(temporary))
    return part


def finalize_progressive_product(directory, expected, info, repo=GITHUB_REPO):
    """After the remote cohort matches the audited product, append release.json last."""
    tag = info['tag']
    ios_signed = any(asset['name'].endswith('-ios.ipa') for asset in info['assets'])
    payload_names = product_payload_names(info['version'], ios_signed)
    manifest_assets = {asset['name']: asset for asset in info['assets']}
    remote = list_release_assets(tag, repo)
    remote_by_name = {asset['name']: asset for asset in remote}
    require(payload_names <= set(remote_by_name), 'cannot finalize before every payload is public')
    extra = set(remote_by_name) - payload_names - {'release.json'}
    require(not extra, 'public release contains unexpected assets')
    staged_manifest = directory / 'release.json'
    require(staged_manifest.is_file(), 'missing release manifest')
    with tempfile.TemporaryDirectory(prefix='vibeface-finalize-') as temporary:
        scratch = Path(temporary)
        for name in sorted(payload_names):
            local = manifest_assets[name]
            confirm_remote_asset(tag, repo, remote_by_name[name], local['sha256'], local['size'], scratch)
        manifest = manifest_assets.get('release.json')
        if manifest:
            require(digest(staged_manifest) == manifest['sha256'] and staged_manifest.stat().st_size == manifest['size'],
                    'release manifest checksum mismatch')
        if 'release.json' in remote_by_name:
            confirm_remote_asset(tag, repo, remote_by_name['release.json'], digest(staged_manifest),
                                 staged_manifest.stat().st_size, scratch)
        else:
            subprocess.run(['gh', 'release', 'upload', tag, f'--repo={repo}', str(staged_manifest)], check=True)
            matches = [asset for asset in list_release_assets(tag, repo) if asset['name'] == 'release.json']
            require(len(matches) == 1, 'uploaded manifest missing after write')
            confirm_remote_asset(tag, repo, matches[0], digest(staged_manifest), staged_manifest.stat().st_size, scratch)
    subprocess.run(['gh', 'release', 'edit', tag, f'--repo={repo}', '--notes', product_complete_notes(info),
                    '--latest=false', f'--prerelease={str(info["channel"] != "stable").lower()}'], check=True)
    return info


def verify_checksum_file(directory, name):
    require((directory / (name + '.sha256')).read_text(encoding='utf-8') == f'{digest(directory / name)}  {name}\n',
            f'checksum file for {name} mismatch')


def verify_product_release(directory, expected):
    """Nested validation of a complete product asset set: every payload, identity and provenance."""
    info = verify_manifest(directory, expected)
    require(info['app'] == 'product', 'release manifest is not the product')
    product_version = info['version']
    source_commit = info['source_commit']
    pins = dict(version=product_version, tag=info['tag'], source_commit=source_commit, channel=info['channel'])
    ios_signed = verify_ios_assets(directory, pins)
    require({p.name for p in directory.iterdir()} == product_asset_names(product_version, ios_signed),
            'product asset set is incomplete or carries extra files')
    with tempfile.TemporaryDirectory(prefix='vibeface-verify-') as tmp:
        tmp = Path(tmp)
        app_archive = f'vibeface-app-{product_version}.tar.gz'
        verify_checksum_file(directory, app_archive)
        verify_component(unpack(directory / app_archive, tmp / 'app'), 'app', dict(pins, target=None))
        for index, target in enumerate(TARGETS):
            bundle_name = f'vibeface-{product_version}-{target}.tar.gz'
            core_name = f'vibeface-core-{product_version}-{target}.tar.gz'
            verify_checksum_file(directory, bundle_name)
            verify_checksum_file(directory, core_name)
            bundle = unpack(directory / bundle_name, tmp / f'cli-{index}')
            build = json.loads((bundle / 'BUILD-INFO.json').read_text(encoding='utf-8'))
            verify_build_info(build)
            require(build['version'] == product_version and build['source_commit'] == source_commit
                    and build['target'] == target, 'CLI bundle version, source or target mismatch')
            for name in program_names(target):
                program = bundle / name
                require(program.is_file() and not program.is_symlink(), 'CLI bundle must contain both native programs')
            require((bundle / 'web').is_dir(), 'CLI bundle must contain the Web App')
            require((bundle / 'web/vibeface_secure.js').is_file() and (bundle / 'web/vibeface_secure_bg.wasm').is_file(),
                    'CLI bundle must contain the browser cryptography SDK')
            content = files(bundle, ('FILES.sha256',))
            require((bundle / 'FILES.sha256').read_text(encoding='utf-8') == ''.join(f'{value}  {name}\n' for name, value in content.items()),
                    f'{target} CLI bundled content changed')
            verify_component(unpack(directory / core_name, tmp / f'core-{index}'), 'core', dict(pins, target=target))
            if target in DESKTOP_TARGETS:
                verify_desktop_receipt(directory, target, pins, digest(directory / bundle_name))
        apk = directory / f'vibeface-{product_version}-android.apk'
        verify_checksum_file(directory, apk.name)
        provenance = json.loads((directory / (apk.name + '.provenance.json')).read_text(encoding='utf-8'))
        require(provenance == mobile_provenance(apk), 'APK provenance mismatch')
        require(provenance['version'] == product_version and provenance['sourceCommit'] == source_commit,
                'APK is not part of this product cohort')
    return info


def verify_www_release(directory, expected):
    """Nested validation of the independent portal asset set."""
    info = verify_manifest(directory, expected)
    require(info['app'] == 'www', 'release manifest is not the portal')
    www_version = info['version']
    require({p.name for p in directory.iterdir()} == www_asset_names(www_version),
            'www asset set is incomplete or carries extra files')
    archive = f'vibeface-www-{www_version}.tar.gz'
    verify_checksum_file(directory, archive)
    with tempfile.TemporaryDirectory(prefix='vibeface-verify-') as tmp:
        verify_component(unpack(directory / archive, Path(tmp) / 'www'), 'www',
                         dict(version=www_version, tag=info['tag'], source_commit=info['source_commit'],
                              channel=info['channel'], target=None))
    return info


def verify_release(release_app, directory, expected):
    """One validator for staged, published and public asset sets."""
    require(release_app in RELEASES, 'release app must be product or www')
    if release_app == 'product':
        return verify_product_release(directory, expected)
    return verify_www_release(directory, expected)


def verify_public(release_app, destination, repo=GITHUB_REPO):
    require(release_app in RELEASES, 'verify-public supports product or www')
    expected = identity(release_app)
    tag = expected['tag']
    view = json.loads(run('gh', 'release', 'view', tag, f'--repo={repo}', '--json', 'isDraft,assets'))
    require(not view['isDraft'], 'release is not public')
    source = json.loads(run('gh', 'api', f'repos/{repo}/commits/{tag}'))['sha']
    require(source == expected['source_commit'], 'public tag source mismatch')
    require(not destination.exists(), 'download destination already exists')
    destination.mkdir(parents=True)
    subprocess.run(['gh', 'release', 'download', tag, f'--repo={repo}', '--dir', str(destination)], check=True)
    names = {asset['name'] for asset in view['assets']}
    require(len(view['assets']) == len(names) and names == expected_asset_names(release_app, expected['version'],
                f'vibeface-{expected["version"]}-ios.ipa' in names),
            'public release contains unexpected, duplicate or missing assets')
    return verify_release(release_app, destination, expected)


def publish(release_app, directory, repo=GITHUB_REPO):
    """The only GitHub Release writer: validate everything, then reconcile the original bytes."""
    require(release_app in RELEASES, 'publish supports product or www')
    require(not run('git', 'status', '--porcelain'), 'release source must be clean')
    expected = identity(release_app)
    info = verify_release(release_app, directory, expected)
    tag = info['tag']
    require(run('git', 'rev-parse', tag + '^{commit}') == info['source_commit'], 'release tag source mismatch')
    subprocess.run(['git', 'merge-base', '--is-ancestor', info['source_commit'], 'origin/main'], check=True)
    if release_app == 'product':
        existing_view = release_view(tag, repo)
        if existing_view is not None and not existing_view['isDraft']:
            ios_signed = f'vibeface-{info["version"]}-ios.ipa' in {asset['name'] for asset in info['assets']}
            payload_names = product_payload_names(info['version'], ios_signed)
            remote = {asset['name'] for asset in existing_view['assets']}
            if (payload_names <= remote and 'release.json' not in remote
                    and (directory / 'release.json').is_file()):
                return finalize_progressive_product(directory, expected, info, repo)
    view = subprocess.run(['gh', 'release', 'view', tag, f'--repo={repo}', '--json', 'isDraft,assets'],
                          capture_output=True, text=True)
    if view.returncode:
        require('release not found' in view.stderr.lower(), 'cannot determine existing release state')
        subprocess.run(['gh', 'release', 'create', tag, f'--repo={repo}', '--verify-tag', '--draft', '--title', tag,
                        '--notes', f'{release_app} {info["version"]}; source {info["source_commit"]}.', '--latest=false',
                        '--prerelease=' + str(info['channel'] != 'stable').lower()], check=True)
        existing = {'isDraft': True, 'assets': []}
    else:
        existing = json.loads(view.stdout)
    staged = {p.name: p for p in directory.iterdir() if p.is_file()}
    names = {a['name'] for a in existing['assets']}
    require(names <= set(staged), 'release contains unexpected assets')
    with tempfile.TemporaryDirectory(dir=directory.parent, prefix='.published-') as tmp:
        tmp = Path(tmp)
        for name in sorted(names):
            subprocess.run(['gh', 'release', 'download', tag, f'--repo={repo}', '--pattern', name, '--dir', str(tmp)], check=True)
            require(digest(tmp / name) == digest(staged[name]), 'existing published bytes differ')
        missing = set(staged) - names
        require(existing['isDraft'] or not missing, 'cannot add to a public release')
        if missing:
            subprocess.run(['gh', 'release', 'upload', tag, f'--repo={repo}',
                            *[str(staged[name]) for name in sorted(missing)]], check=True)
        for name in sorted(missing):
            subprocess.run(['gh', 'release', 'download', tag, f'--repo={repo}', '--pattern', name, '--dir', str(tmp)], check=True)
            require(digest(tmp / name) == digest(staged[name]), 'uploaded bytes differ')
    if existing['isDraft']:
        subprocess.run(['gh', 'release', 'edit', tag, f'--repo={repo}', '--draft=false', '--latest=false',
                        '--prerelease=' + str(info['channel'] != 'stable').lower()], check=True)
    return info


def producer_manifest(directory, pins):
    path = directory / 'release.json'
    require(path.is_file(), 'missing producer release.json')
    info = json.loads(path.read_text(encoding='utf-8'))
    verify_release_identity(info)
    require(all(info[key] == value for key, value in pins.items()), 'producer release identity mismatch')
    return info


def assemble_product(input_root, output):
    """Collect every product producer directory into one complete, fully validated asset set."""
    input_root = input_root.resolve()
    require(not run('git', 'status', '--porcelain'), 'release source must be clean')
    expected = identity('product')
    product_version = expected['version']
    pins = {key: expected[key] for key in ('app', 'version', 'tag', 'source_commit', 'channel')}
    require({p.name for p in input_root.iterdir()} == {'app', 'native', 'mobile', 'desktop', 'ios'},
            'input-root must contain exactly app, native, mobile, desktop and ios')
    native_root = input_root / 'native'
    desktop_root = input_root / 'desktop'
    require({p.name for p in native_root.iterdir()} == set(TARGETS), 'native must contain one directory per target')
    require({p.name for p in desktop_root.iterdir()} == set(DESKTOP_TARGETS),
            'desktop must contain one directory per desktop target')
    producers = [(input_root / 'app', app_asset_names(product_version)),
                 (input_root / 'mobile', apk_asset_names(product_version))]
    ios_signed = verify_ios_assets(input_root / 'ios', pins)
    producers.append((input_root / 'ios', ios_asset_names(product_version, ios_signed)))
    for target in TARGETS:
        producers.append((native_root / target, native_asset_names(product_version, target)))
    for target in DESKTOP_TARGETS:
        producers.append((desktop_root / target, desktop_asset_names(product_version, target)))
    seen = {}
    for directory, payload in producers:
        producer_manifest(directory, pins)
        require({p.name for p in directory.iterdir()} == payload | {'release.json'},
                f'producer {directory.name} file set is incomplete or carries extra files')
        for name in sorted(payload):
            if name + '.sha256' in payload:
                verify_checksum_file(directory, name)
                value = digest(directory / name)
                require(seen.get(value, name) == name, 'two producers staged identical bytes')
                seen[value] = name
    output.mkdir(parents=True, exist_ok=True)
    require(not any(output.iterdir()), 'output directory must be empty')
    for directory, payload in producers:
        for name in sorted(payload):
            require(not (output / name).exists(), 'producers must not overwrite assembled bytes')
            shutil.copy2(directory / name, output / name)
    manifest('product', output)
    return verify_product_release(output, expected)


def plan(release_app, tag=None, require_main=False, publishing=False, github_output=None):
    require(release_app in RELEASES, 'plan --app product|www')
    data = identity(release_app)
    if tag:
        kind, value, channel = parse_tag(tag)
        require(kind == release_app, 'tag kind must match --app')
        require((tag, value, channel) == (data['tag'], data['version'], data['channel']),
                'tag does not match the declared version')
    if require_main or publishing:
        require(not run('git', 'status', '--porcelain'), 'release source must be clean')
        subprocess.run(['git', 'merge-base', '--is-ancestor', 'HEAD', 'origin/main'], check=True)
    if publishing:
        require(run('git', 'rev-parse', data['tag'] + '^{commit}') == data['source_commit'],
                'publishing requires the existing release tag at this source commit')
    if github_output:
        with open(github_output, 'a') as f:
            for key, value in data.items():
                f.write(f'{key}={value}\n')
    print(json.dumps(data))
    return data


def self_test():
    from unittest.mock import patch
    real_run = run
    product_version = canonical_product_version()
    channel = version(product_version)
    www_version = read_version('www')
    product_tag = f'v{product_version}'
    www_tag = f'www-v{www_version}'
    source = 'a' * 40
    foreign = 'b' * 40
    counts = {'accepted': 0, 'rejected': 0}

    def accepts(action):
        result = action()
        counts['accepted'] += 1
        return result

    def rejects(label, action, expect=None):
        try:
            action()
        except (ValueError, KeyError, OSError, json.JSONDecodeError) as error:
            require(expect is None or expect in str(error), f'{label} was rejected for the wrong reason: {error}')
        else:
            raise AssertionError(f'accepted {label}')
        counts['rejected'] += 1

    with tempfile.TemporaryDirectory(prefix='vibeface-release-utf8-') as temporary:
        root = Path(temporary)
        mobile = root / 'apps/mobile'
        mobile.mkdir(parents=True)
        (mobile / 'app.json').write_bytes(json.dumps({
            'expo': {'name': '浏览', 'extra': {'releaseVersion': '1.2.3-nightly.20261107.1'}},
        }, ensure_ascii=False).encode('utf-8'))
        original_read_text = Path.read_text

        def windows_default_read(path, *args, **kwargs):
            if not args and 'encoding' not in kwargs:
                kwargs['encoding'] = 'cp1252'
            return original_read_text(path, *args, **kwargs)

        with patch.object(Path, 'read_text', windows_default_read):
            accepts(lambda: require(read_version('mobile', root) == '1.2.3-nightly.20261107.1',
                                    'UTF-8 mobile configuration must ignore the host default codepage'))

    with tempfile.TemporaryDirectory(prefix='vibeface-release-order-') as temporary:
        root = Path(temporary)
        (root / 'README.md').write_bytes(b'readme')
        (root / 'docs').mkdir()
        (root / 'docs/node.md').write_bytes(b'node')
        with patch.object(type(root), '__lt__',
                          lambda left, right: PureWindowsPath(str(left)) < PureWindowsPath(str(right))):
            accepts(lambda: require(list(files(root)) == ['README.md', 'docs/node.md'],
                                    'sealed file order must ignore host path comparison rules'))

    accepts(lambda: require(parse_tag(product_tag) == ('product', product_version, channel), 'product tag'))
    accepts(lambda: require(parse_tag(www_tag) == ('www', www_version, version(www_version)), 'www tag'))
    for bad in ('v01.0.1', 'v1', 'www-v', 'app-v0.0.1', 'cli-v0.0.1', 'mobile-v0.0.1',
                'v0.0.1-nightly.20260229.1', 'v1.0.0+extra'):
        rejects(f'invalid tag {bad}', lambda bad=bad: parse_tag(bad))

    expo = json.loads((ROOT / 'apps/mobile/app.json').read_text(encoding='utf-8'))['expo']
    badging = (f"package: name='{expo['android']['package']}' versionCode='{expo['android']['versionCode']}'"
               f" versionName='{expo['version']}'")
    public_view = {'directory': None, 'draft': False, 'source': source}

    def boundary(*args, **kwargs):
        """Fake only external boundaries: this checkout's git, the Android aapt tool and gh reads."""
        if args[0] == 'git':
            if args[1] == 'rev-parse':
                return source
            if args[1] == 'status':
                return ''
            raise AssertionError(args)
        if args[0] == 'aapt':
            require(args[1:3] == ('dump', 'badging'), 'unexpected aapt call')
            return badging
        if args[:3] == ('gh', 'release', 'view'):
            return json.dumps(dict(isDraft=public_view['draft'],
                                   assets=[{'name': p.name} for p in sorted(public_view['directory'].iterdir())]))
        if args[:2] == ('gh', 'api'):
            return json.dumps(dict(sha=public_view['source']))
        return real_run(*args, **kwargs)

    scratch = ROOT / '.artifacts/release-tests'
    scratch.mkdir(parents=True, exist_ok=True)
    # The per-platform user service installers are repository files; the real compose job proves they
    # exist, so the self-test composes against fixtures instead of the checkout's scripts directory.
    installers = scratch / 'service-installers'
    installers.mkdir(exist_ok=True)
    for installer_name in ('install-node-service.sh', 'install-node-service.ps1'):
        (installers / installer_name).write_text('# service installer fixture\n')

    def fixture_installer(target):
        return installers / ('install-node-service.ps1' if target in WINDOWS_TARGETS else 'install-node-service.sh')

    with tempfile.TemporaryDirectory(dir=scratch) as tmp, \
            patch(f'{__name__}.run', side_effect=boundary), patch(f'{__name__}.shutil.which', return_value='aapt'), \
            patch(f'{__name__}.service_installer', side_effect=fixture_installer):
        tmp = Path(tmp)

        def repoint(path, **changes):
            write_json(path, json.loads(path.read_text(encoding='utf-8')) | changes)

        # Identity: one public product identity, one independent portal identity.
        product = accepts(lambda: identity('product'))
        original_read_version = read_version

        def changed_desktop_version(app, root=None):
            return '9.9.9' if app == 'desktop' else original_read_version(app, root)

        with patch(f'{__name__}.read_version', side_effect=changed_desktop_version):
            rejects('a desktop version outside the product cohort', lambda: identity('product'),
                    'desktop version must equal')
        portal = accepts(lambda: identity('www'))
        require(product['tag'] == product_tag and product['source_commit'] == source, 'product identity')
        require(portal['tag'] == www_tag and portal['app'] == 'www', 'independent portal identity')
        require(component_identity('core') == dict(product, app='core'), 'Core carries the product identity')
        require(component_identity('app') == dict(product, app='app'), 'App carries the product identity')
        require(component_identity('www') == dict(portal, app='www'), 'portal component carries the portal identity')
        for bad in ('mobile', 'cli', 'app', 'core'):
            rejects(f'release identity for {bad}', lambda bad=bad: identity(bad))
        rejects('component identity for mobile', lambda: component_identity('mobile'))

        with contextlib.redirect_stdout(io.StringIO()):
            accepts(lambda: plan('product', tag=product_tag))
            accepts(lambda: plan('product'))
            accepts(lambda: plan('www', tag=www_tag))
            rejects('portal tag on the product plan', lambda: plan('product', tag=www_tag))
            rejects('undeclared product version tag', lambda: plan('product', tag='v9.9.9'))
            rejects('mobile plan', lambda: plan('mobile'))

        # Portal producer, published independently of the product.
        www_dir = tmp / 'www-release'
        site_source = tmp / 'site-source'
        site_source.mkdir()
        (site_source / 'index.html').write_text('<!doctype html>portal fixture')
        accepts(lambda: pack_component('www', site_source, www_dir))
        www_archive = www_dir / f'vibeface-www-{www_version}.tar.gz'
        accepts(lambda: verify_www_release(www_dir, portal))
        rejects('portal assets validated as the product', lambda: verify_product_release(www_dir, portal))

        # App producer: a real component archive plus its product partial manifest.
        collected = tmp / 'collect'
        app_dir = collected / 'app'
        web_source = tmp / 'web-source'
        (web_source / 'assets').mkdir(parents=True)
        (web_source / 'index.html').write_text('<!doctype html>app fixture')
        (web_source / 'assets/app.js').write_text('export const fixture = true;\n')
        (web_source / 'vibeface_secure.js').write_text('export const secure = true;\n')
        (web_source / 'vibeface_secure_bg.wasm').write_bytes(b'\x00asm\x01\x00\x00\x00')
        accepts(lambda: pack_component('app', web_source, app_dir))
        app_archive = app_dir / f'vibeface-app-{product_version}.tar.gz'
        require(json.loads((app_dir / 'release.json').read_text(encoding='utf-8'))['app'] == 'product',
                'the App producer stages the product release')

        # Native staging: real executable fixtures reporting the cohort version, per platform basename.
        def native_programs(directory, target, reported=product_version):
            directory.mkdir(parents=True, exist_ok=True)
            for name in program_names(target):
                binary = directory / name
                binary.write_text(f'#!/bin/sh\necho "{name.removesuffix(".exe")} {reported}"\n')
                binary.chmod(0o755)
            return directory

        native_bin = {target: native_programs(tmp / f'native-bin-{target}', target) for target in TARGETS}
        wrong_bin = native_programs(tmp / 'wrong-bin', TARGETS[0], '9.9.9')
        for target in TARGETS:
            receipt = accepts(lambda target=target: stage_native(target, native_bin[target], tmp / f'stage-{target}'))
            require(receipt['target'] == target and receipt['source_commit'] == source
                    and receipt['version'] == product_version, 'native receipt identity')
            require(type(receipt['schema']) is int and type(receipt['data_epoch']) is int, 'native receipt integer types')
            require({p.name for p in (tmp / f'stage-{target}').iterdir()}
                    == {*program_names(target), 'NATIVE-INFO.json'}, 'native staging holds exactly three files')
        require(program_names('x86_64-pc-windows-msvc') == ('vibeface.exe', 'vibeface-core.exe'),
                'Windows bundles keep .exe basenames')
        rejects('native programs of another version', lambda: stage_native(TARGETS[0], wrong_bin, tmp / 'stage-wrong'))
        rejects('unknown native target', lambda: stage_native('mips-unknown', native_bin[TARGETS[0]], tmp / 'stage-mips'))
        rejects('Unix basenames staged as the Windows platform',
                lambda: stage_native('x86_64-pc-windows-msvc', native_bin[TARGETS[0]], tmp / 'stage-unix-as-windows'))
        incomplete_bin = tmp / 'incomplete-bin'
        incomplete_bin.mkdir()
        shutil.copy2(native_bin[TARGETS[0]] / 'vibeface', incomplete_bin / 'vibeface')
        rejects('native input without Core', lambda: stage_native(TARGETS[0], incomplete_bin, tmp / 'stage-incomplete'))
        rejects('restaging over an existing staging directory',
                lambda: stage_native(TARGETS[0], native_bin[TARGETS[0]], tmp / f'stage-{TARGETS[0]}'), 'must be empty')

        def staged_copy(label, mutate):
            destination = tmp / f'stage-{label}'
            shutil.copytree(tmp / f'stage-{TARGETS[0]}', destination)
            mutate(destination)
            return destination

        def compose_into(label, native_dir, archive=None, target=TARGETS[0]):
            return compose_offline(target, native_dir, archive or app_archive, tmp / f'compose-{label}')

        for target in TARGETS:
            accepts(lambda target=target: compose_into(target, tmp / f'stage-{target}', target=target))
            shutil.copytree(tmp / f'compose-{target}', collected / 'native' / target)
            require({p.name for p in (collected / 'native' / target).iterdir()}
                    == native_asset_names(product_version, target) | {'release.json'}, 'compose output set')
        bundle_name = f'vibeface-{product_version}-{TARGETS[0]}.tar.gz'

        # Desktop producers: installer and archive payloads bound to this platform's released runtime.
        def stage_desktop(destination, target, **changes):
            destination.mkdir(parents=True, exist_ok=True)
            runtime = collected / 'native' / target / f'vibeface-{product_version}-{target}.tar.gz'
            artifacts = {}
            for name in desktop_payload_names(product_version, target):
                payload = destination / name
                payload.write_bytes(f'{name} fixture payload'.encode())
                payload.with_name(name + '.sha256').write_text(f'{digest(payload)}  {name}\n')
                artifacts[name] = digest(payload)
            receipt = dict(schema=SCHEMA_VERSION, app='desktop', target=target, version=product_version,
                           tag=product_tag, channel=channel, source_commit=source, electron=ELECTRON_VERSION,
                           signed=target == 'aarch64-apple-darwin',
                           signing='developer-id-notarized' if target == 'aarch64-apple-darwin'
                           else 'platform-checksums' if target.endswith('-unknown-linux-gnu')
                           else 'unsigned-development-preview',
                           runtime={'archive': runtime.name, 'sha256': digest(runtime)}, artifacts=artifacts)
            receipt.update(changes)
            write_json(destination / f'{desktop_basename(product_version, target)}.provenance.json', receipt)
            return destination

        desktop_pins = dict(version=product_version, tag=product_tag, source_commit=source, channel=channel)
        for target in DESKTOP_TARGETS:
            produced = stage_desktop(collected / 'desktop' / target, target)
            runtime_digest = digest(collected / 'native' / target / f'vibeface-{product_version}-{target}.tar.gz')
            info = accepts(lambda produced=produced, target=target, runtime_digest=runtime_digest:
                           verify_desktop_receipt(produced, target, desktop_pins, runtime_digest))
            require(info['electron'] == ELECTRON_VERSION and len(info['artifacts']) == 2, 'desktop receipt payloads')
            accepts(lambda produced=produced: manifest('product', produced))
            require({p.name for p in produced.iterdir()}
                    == desktop_asset_names(product_version, target) | {'release.json'}, 'desktop producer output set')
        require(set(desktop_payload_names(product_version, 'x86_64-pc-windows-msvc'))
                == {f'vibeface-desktop-{product_version}-x86_64-pc-windows-msvc.exe',
                    f'vibeface-desktop-{product_version}-x86_64-pc-windows-msvc.zip'},
                'Windows desktop ships a per-user installer and an archive')

        def desktop_variant(label, target, **changes):
            return stage_desktop(tmp / f'desktop-{label}', target, **changes)

        mac_target, windows_target = 'aarch64-apple-darwin', 'x86_64-pc-windows-msvc'
        mac_runtime = digest(collected / 'native' / mac_target / f'vibeface-{product_version}-{mac_target}.tar.gz')
        for linux_target in ('x86_64-unknown-linux-gnu', 'aarch64-unknown-linux-gnu'):
            accepts(lambda target=linux_target: verify_desktop_receipt(
                desktop_variant('stable-' + target, target, channel='stable'), target,
                dict(desktop_pins, channel='stable')))
            rejects('Linux cannot borrow a macOS signature claim', lambda target=linux_target:
                    verify_desktop_receipt(desktop_variant('signed-' + target, target,
                        signed=True, signing='developer-id-notarized'), target, desktop_pins),
                    'Linux platform checksums must match')
        rejects('Windows cannot use Linux checksums instead of a signature', lambda:
                verify_desktop_receipt(desktop_variant('windows-linux-claim', windows_target,
                    signing='platform-checksums'), windows_target, desktop_pins),
                'Linux platform checksums must match')
        rejects('an unsigned desktop payload for a stable channel',
                lambda: verify_desktop_receipt(
                    desktop_variant('stable-unsigned', windows_target, channel='stable'), windows_target,
                    dict(desktop_pins, channel='stable')),
                'stable desktop distribution requires a signed application')
        rejects('a desktop receipt claiming another Electron version',
                lambda: verify_desktop_receipt(desktop_variant('other-electron', mac_target, electron='34.5.8'),
                                               mac_target, desktop_pins),
                'pinned Electron version')
        rejects('a signed flag without a signing kind',
                lambda: verify_desktop_receipt(
                    desktop_variant('claimed-signed', windows_target, signed=True), windows_target, desktop_pins),
                'signing status and signed flag disagree')
        rejects('Authenticode claimed for the macOS application',
                lambda: verify_desktop_receipt(
                    desktop_variant('wrong-kind', mac_target, signing='authenticode'), mac_target, desktop_pins),
                'signing kind does not match')
        rejects('a desktop build from another commit',
                lambda: verify_desktop_receipt(desktop_variant('other-commit', mac_target, source_commit=foreign),
                                               mac_target, desktop_pins),
                'cohort mismatch')
        rejects('a desktop bundle built on a foreign runtime',
                lambda: verify_desktop_receipt(
                    desktop_variant('foreign-runtime', mac_target,
                                    runtime={'archive': f'vibeface-{product_version}-{mac_target}.tar.gz',
                                             'sha256': '0' * 64}),
                    mac_target, desktop_pins, mac_runtime),
                'differs from the released CLI bundle')
        rejects('a desktop receipt naming another platform runtime',
                lambda: verify_desktop_receipt(
                    desktop_variant('other-runtime', mac_target,
                                    runtime={'archive': f'vibeface-{product_version}-{windows_target}.tar.gz',
                                             'sha256': mac_runtime}),
                    mac_target, desktop_pins),
                'must be this platform CLI bundle')
        def changed_payload():
            produced = desktop_variant('changed-payload', mac_target)
            (produced / f'{desktop_basename(product_version, mac_target)}.dmg').write_bytes(b'changed')
            return verify_desktop_receipt(produced, mac_target, desktop_pins)

        rejects('a changed desktop payload', changed_payload, 'desktop payload digest mismatch')
        rejects('an extra desktop receipt field',
                lambda: verify_desktop_receipt(desktop_variant('extra-field', mac_target, legacy=True),
                                               mac_target, desktop_pins),
                'unsupported desktop receipt fields')

        def set_receipt(destination, **changes):
            repoint(destination / 'NATIVE-INFO.json', **changes)

        rejects('native staging for another target',
                lambda: compose_into('other-target', tmp / f'stage-{TARGETS[1]}'), 'target mismatch')
        rejects('native staging from another commit',
                lambda: compose_into('other-commit', staged_copy('other-commit', lambda d: set_receipt(d, source_commit=foreign))),
                'must match this checkout')
        rejects('native staging of another version',
                lambda: compose_into('other-version', staged_copy('other-version', lambda d: set_receipt(d, version='9.9.9'))),
                'version mismatch')
        rejects('native receipt with a text data epoch',
                lambda: compose_into('text-epoch', staged_copy('text-epoch', lambda d: set_receipt(d, data_epoch='1'))))
        rejects('extra file beside the staged programs',
                lambda: compose_into('extra', staged_copy('extra', lambda d: (d / 'extra.txt').write_text('x'))))
        rejects('changed staged program',
                lambda: compose_into('changed', staged_copy('changed', lambda d: (d / 'vibeface').write_text('#!/bin/sh\necho changed\n'))))

        def symlinked(destination):
            (destination / 'vibeface').unlink()
            (destination / 'vibeface').symlink_to(destination / 'vibeface-core')
        rejects('symlinked staged program', lambda: compose_into('symlink', staged_copy('symlink', symlinked)), 'symlink')

        foreign_component = tmp / 'foreign-app' / f'vibeface-app-{product_version}'
        (foreign_component / 'web').mkdir(parents=True)
        (foreign_component / 'web/index.html').write_text('foreign app fixture')
        write_json(foreign_component / 'COMPONENT-INFO.json',
                   dict(app='app', version=product_version, tag=product_tag, source_commit=foreign, channel=channel,
                        schema=SCHEMA_VERSION, target=None, files=files(foreign_component)))
        foreign_archive = foreign_component.parent / f'vibeface-app-{product_version}.tar.gz'
        public_archive(foreign_component, foreign_archive)
        rejects('App component from another commit',
                lambda: compose_into('foreign-app', tmp / f'stage-{TARGETS[0]}', foreign_archive), 'component pin mismatch')
        rejects('App and native staging agreeing on another commit',
                lambda: compose_into('foreign-pair', staged_copy('pair', lambda d: set_receipt(d, source_commit=foreign)),
                                     foreign_archive), 'must match this checkout')
        rejects('portal component composed as the App',
                lambda: compose_into('portal-app', tmp / f'stage-{TARGETS[0]}', www_archive))

        # Android producer, staged as the product rather than its own release.
        mobile_dir = collected / 'mobile'
        mobile_dir.mkdir(parents=True)
        apk = mobile_dir / f'vibeface-{product_version}-android.apk'
        apk.write_bytes(b'APK fixture payload')
        provenance = accepts(lambda: mobile_provenance(apk))
        write_json(apk.with_name(apk.name + '.provenance.json'), provenance)
        apk.with_name(apk.name + '.sha256').write_text(f'{digest(apk)}  {apk.name}\n')
        require(provenance['sha256'] == digest(apk) and provenance['sourceCommit'] == source
                and provenance['version'] == product_version, 'APK provenance')
        accepts(lambda: manifest('product', mobile_dir))
        require(json.loads((mobile_dir / 'release.json').read_text(encoding='utf-8'))['app'] == 'product',
                'the Android producer stages the product release')
        rejects('standalone mobile release manifest', lambda: manifest('mobile', mobile_dir))
        for bad in (badging.replace(expo['android']['package'], 'other.package'),
                    badging.replace(f"versionCode='{expo['android']['versionCode']}'", "versionCode='999999'"),
                    badging.replace(f"versionName='{expo['version']}'", "versionName='9.9.9'"),
                    'not an Android package'):
            def foreign_badging(bad=bad):
                def handler(*args, **kwargs):
                    return bad if args[0] == 'aapt' else boundary(*args, **kwargs)
                with patch(f'{__name__}.run', side_effect=handler):
                    mobile_provenance(apk)
            rejects('foreign APK identity', foreign_badging)

        ios_dir = collected / 'ios'
        ios_dir.mkdir()
        ios_name = f'vibeface-{product_version}-ios-device-unsigned.xcarchive.zip'
        ios_info = dict(CFBundleIdentifier=expo['ios']['bundleIdentifier'], CFBundleShortVersionString=expo['version'],
                        CFBundleVersion=expo['ios']['buildNumber'], CFBundleSupportedPlatforms=['iPhoneOS'])
        with zipfile.ZipFile(ios_dir / ios_name, 'w') as archive:
            archive.writestr('Vibeface.xcarchive/Products/Applications/Vibeface.app/Info.plist', plistlib.dumps(ios_info))
        (ios_dir / (ios_name + '.sha256')).write_text(f'{digest(ios_dir / ios_name)}  {ios_name}\n')
        ios_receipt = dict(schema=1, platform='ios-device', source_sha=source, release_version=product_version,
            bundle_identifier=expo['ios']['bundleIdentifier'], bundle_short_version=expo['version'],
            build_number=expo['ios']['buildNumber'], artifact=ios_name, sha256=digest(ios_dir / ios_name),
            signed=False, installable=False, kind='unsigned-xcarchive-inspection-only')
        write_json(ios_dir / (ios_name + '.provenance.json'), ios_receipt)
        accepts(lambda: verify_ios_assets(ios_dir, desktop_pins))
        accepts(lambda: manifest('product', ios_dir))

        for field, changed in [('source_sha', foreign), ('installable', True), ('signed', True)]:
            ios_receipt_path = ios_dir / (ios_name + '.provenance.json')
            write_json(ios_receipt_path, ios_receipt | {field: changed})
            rejects('an invalid iOS archive claim for ' + field, lambda: verify_ios_assets(ios_dir, desktop_pins))
            write_json(ios_receipt_path, ios_receipt)
        for method in ('ad-hoc', 'app-store-connect'):
            signed_dir = tmp / ('ios-' + method)
            signed_dir.mkdir()
            signed_name = f'vibeface-{product_version}-ios-device-signed.xcarchive.zip'
            shutil.copy2(ios_dir / ios_name, signed_dir / signed_name)
            with zipfile.ZipFile(signed_dir / signed_name, 'a') as archive:
                archive.writestr('Vibeface.xcarchive/Products/Applications/Vibeface.app/embedded.mobileprovision', b'signed profile fixture')
            (signed_dir / (signed_name + '.sha256')).write_text(f'{digest(signed_dir / signed_name)}  {signed_name}\n')
            write_json(signed_dir / (signed_name + '.provenance.json'), ios_receipt | {
                'artifact': signed_name, 'sha256': digest(signed_dir / signed_name), 'signed': True, 'kind': 'signed-xcarchive-inspection-only'})
            ipa_name = f'vibeface-{product_version}-ios.ipa'
            with zipfile.ZipFile(signed_dir / ipa_name, 'w') as archive:
                archive.writestr('Payload/Vibeface.app/Info.plist', plistlib.dumps(ios_info))
                archive.writestr('Payload/Vibeface.app/embedded.mobileprovision', b'signed profile fixture')
            (signed_dir / (ipa_name + '.sha256')).write_text(f'{digest(signed_dir / ipa_name)}  {ipa_name}\n')
            ipa_receipt = ios_receipt | dict(artifact=ipa_name, sha256=digest(signed_dir / ipa_name), signed=True,
                installable=method == 'ad-hoc', testflight_eligible=method == 'app-store-connect',
                kind='signed-ios-ipa', distribution_method=method, team_id='TESTTEAM00',
                provisioning_profile_uuid='profile-fixture', provisioning_profile_expiration='2099-01-01T00:00:00')
            write_json(signed_dir / (ipa_name + '.provenance.json'), ipa_receipt)
            accepts(lambda: verify_ios_assets(signed_dir, desktop_pins))
            write_json(signed_dir / (ipa_name + '.provenance.json'), ipa_receipt | {'installable': method != 'ad-hoc'})
            rejects('an IPA claiming the wrong installation method', lambda: verify_ios_assets(signed_dir, desktop_pins),
                    'iOS installability claim mismatch')

        # Assembly: the complete product, then every way a producer set can be wrong.
        assembled = tmp / 'assembled'
        info = accepts(lambda: assemble_product(collected, assembled))
        require({p.name for p in assembled.iterdir()} == product_asset_names(product_version), 'assembled asset set')
        require(len(info['assets']) == 44 and len(product_asset_names(product_version)) == 45,
                'the product publishes exactly 44 payloads plus its manifest')
        rejects('assembling into a populated output', lambda: assemble_product(collected, assembled))

        def assemble(label, mutate):
            tree = tmp / f'collect-{label}'
            shutil.copytree(collected, tree)
            mutate(tree)
            assemble_product(tree, tmp / f'assembled-{label}')

        rejects('mobile producer claiming its own release',
                lambda: assemble('mobile-app', lambda d: repoint(d / 'mobile/release.json', app='mobile')))
        rejects('producer from another commit',
                lambda: assemble('other-commit', lambda d: repoint(d / 'app/release.json', source_commit=foreign)))
        rejects('producer of another version',
                lambda: assemble('other-version', lambda d: repoint(d / f'native/{TARGETS[1]}/release.json',
                                                                    version='9.9.9', tag='v9.9.9')))
        rejects('missing native producer',
                lambda: assemble('missing-native', lambda d: shutil.rmtree(d / f'native/{TARGETS[2]}')))
        rejects('missing producer payload',
                lambda: assemble('missing-file',
                                 lambda d: (d / 'app' / f'vibeface-app-{product_version}.tar.gz.sha256').unlink()))
        rejects('extra file in a producer directory',
                lambda: assemble('extra-file', lambda d: (d / 'mobile/notes.txt').write_text('extra')))
        rejects('extra producer directory', lambda: assemble('extra-producer', lambda d: (d / 'extras').mkdir()))
        rejects('missing desktop producer',
                lambda: assemble('missing-desktop', lambda d: shutil.rmtree(d / f'desktop/{DESKTOP_TARGETS[1]}')))
        rejects('desktop producer for an unreleased platform',
                lambda: assemble('other-desktop', lambda d: (d / f'desktop/{TARGETS[1]}').mkdir()))
        rejects('desktop payload swapped between platforms',
                lambda: assemble('swapped-desktop', lambda d: shutil.copy2(
                    d / f'desktop/{DESKTOP_TARGETS[0]}/{desktop_basename(product_version, DESKTOP_TARGETS[0])}.zip',
                    d / f'desktop/{DESKTOP_TARGETS[1]}/{desktop_basename(product_version, DESKTOP_TARGETS[1])}.zip')))
        rejects('corrupt staged archive',
                lambda: assemble('corrupt', lambda d: (d / 'native' / TARGETS[0] / bundle_name).write_bytes(b'corrupt')))
        rejects('native payload of another target',
                lambda: assemble('other-target', lambda d: shutil.copy2(
                    d / 'native' / TARGETS[1] / f'vibeface-{product_version}-{TARGETS[1]}.tar.gz',
                    d / 'native' / TARGETS[0] / bundle_name)))
        rejects('foreign APK bytes',
                lambda: assemble('foreign-apk', lambda d: (d / 'mobile' / apk.name).write_bytes(b'other APK payload')))

        def forge_producer_provenance(directory):
            """A producer that restaged its own checksums and manifest around a foreign payload."""
            path = directory / 'mobile' / (apk.name + '.provenance.json')
            write_json(path, json.loads(path.read_text(encoding='utf-8')) | {'sourceCommit': foreign})
            manifest('product', directory / 'mobile')

        def swap_producer_app(directory):
            name = f'vibeface-app-{product_version}.tar.gz'
            shutil.copy2(foreign_archive, directory / 'app' / name)
            (directory / 'app' / (name + '.sha256')).write_text(f'{digest(directory / "app" / name)}  {name}\n')
            manifest('product', directory / 'app')

        rejects('restaged APK provenance from another commit',
                lambda: assemble('forged-apk', forge_producer_provenance), 'APK provenance mismatch')
        rejects('restaged App component from another commit',
                lambda: assemble('forged-app', swap_producer_app), 'component pin mismatch')

        # Publication: full validation first, then idempotent reconciliation of the original bytes.
        def remote(assets=(), exists=False, draft=True, stderr='release not found'):
            return {'exists': exists, 'draft': draft, 'uploaded': [], 'stderr': stderr,
                    'assets': {name: (assembled / name).read_bytes() for name in assets}}

        def publisher(state, calls):
            def handler(*args, **kwargs):
                command = list(args[0])
                if command[:2] == ['git', 'merge-base']:
                    calls.append('merge-base')
                    return subprocess.CompletedProcess(command, 0)
                require(command[0] == 'gh', 'unexpected external command')
                action = command[2]
                calls.append(action)
                if action == 'view':
                    if not state['exists']:
                        return subprocess.CompletedProcess(command, 1, '', state['stderr'])
                    return subprocess.CompletedProcess(command, 0, json.dumps(dict(
                        isDraft=state['draft'], assets=[{'name': n} for n in sorted(state['assets'])])), '')
                if action == 'create':
                    state.update(exists=True, draft=True)
                elif action == 'upload':
                    for path in map(Path, command[5:]):
                        state['uploaded'].append(path.name)
                        state['assets'][path.name] = path.read_bytes()
                elif action == 'download':
                    name = command[command.index('--pattern') + 1]
                    destination = Path(command[command.index('--dir') + 1])
                    (destination / name).write_bytes(state['assets'][name])
                elif action == 'edit':
                    state['draft'] = False
                else:
                    raise AssertionError(command)
                return subprocess.CompletedProcess(command, 0)
            return handler

        def staged_release(label, mutate=lambda directory: None):
            destination = tmp / f'release-{label}' / 'assets'
            shutil.copytree(assembled, destination)
            mutate(destination)
            return destination

        def publish_product(directory, state, calls):
            with patch(f'{__name__}.subprocess.run', side_effect=publisher(state, calls)):
                return publish('product', directory)

        complete = product_asset_names(product_version)
        fresh, calls = remote(), []
        accepts(lambda: publish_product(staged_release('fresh'), fresh, calls))
        require(set(fresh['assets']) == complete and not fresh['draft'],
                'a fresh publication uploads the complete set and then flips the draft')
        require(calls.count('create') == 1 and calls.count('edit') == 1 and calls[0] == 'merge-base',
                'the publisher checks main ancestry before writing and flips once')

        present = sorted(complete)[:5]
        resumed, calls = remote(present, exists=True), []
        accepts(lambda: publish_product(staged_release('resume'), resumed, calls))
        require(sorted(resumed['uploaded']) == sorted(complete - set(present)),
                'a resumed publication uploads only the missing original bytes')
        require(not resumed['draft'] and 'create' not in calls, 'a resumed publication reuses the existing draft')

        conflicted, calls = remote(complete, exists=True), []
        conflicted['assets']['release.json'] = b'{"app": "product"}\n'
        rejects('conflicting published bytes',
                lambda: publish_product(staged_release('conflict'), conflicted, calls), 'existing published bytes differ')
        require('upload' not in calls and 'edit' not in calls, 'an unknown byte conflict must not write')

        public_partial, calls = remote(present, exists=True, draft=False), []
        rejects('adding assets to a public release',
                lambda: publish_product(staged_release('public-partial'), public_partial, calls),
                'cannot add to a public release')
        require('upload' not in calls, 'a public release must not gain assets')

        unknown, calls = remote(stderr='could not resolve host api.github.com'), []
        rejects('an unknown remote outcome treated as a missing release',
                lambda: publish_product(staged_release('unknown'), unknown, calls),
                'cannot determine existing release state')
        require('create' not in calls and 'upload' not in calls, 'an unknown remote state must not write')

        republished, calls = remote(complete, exists=True, draft=False), []
        accepts(lambda: publish_product(staged_release('republish'), republished, calls))
        require(not republished['uploaded'] and 'edit' not in calls,
                'republishing identical bytes changes nothing')

        def swap_in_foreign_app(directory):
            """A foreign payload under a freshly signed manifest: only nested validation can see it."""
            name = f'vibeface-app-{product_version}.tar.gz'
            shutil.copy2(foreign_archive, directory / name)
            (directory / (name + '.sha256')).write_text(f'{digest(directory / name)}  {name}\n')
            manifest('product', directory)

        def forge_apk_provenance(directory):
            path = directory / (apk.name + '.provenance.json')
            write_json(path, json.loads(path.read_text(encoding='utf-8')) | {'sourceCommit': foreign})
            manifest('product', directory)

        def forge_desktop_runtime(directory):
            """A desktop installer rebuilt on a runtime that this release never published."""
            path = directory / f'{desktop_basename(product_version, DESKTOP_TARGETS[0])}.provenance.json'
            receipt = json.loads(path.read_text(encoding='utf-8'))
            write_json(path, receipt | {'runtime': dict(receipt['runtime'], sha256='0' * 64)})
            manifest('product', directory)

        for label, mutate in (
                ('short', lambda d: (d / f'vibeface-core-{product_version}-{TARGETS[0]}.tar.gz').unlink()),
                ('extra', lambda d: (d / 'extra.txt').write_text('x')),
                ('foreign', lambda d: repoint(d / 'release.json', source_commit=foreign)),
                ('corrupt', lambda d: (d / f'vibeface-app-{product_version}.tar.gz').write_bytes(b'corrupt')),
                ('foreign App component', swap_in_foreign_app),
                ('forged APK provenance', forge_apk_provenance),
                ('forged desktop runtime receipt', forge_desktop_runtime),
                ('desktop set missing its installer',
                 lambda d: (d / f'{desktop_basename(product_version, DESKTOP_TARGETS[1])}.exe').unlink()),
        ):
            blocked, calls = remote(), []
            rejects(f'publishing a {label} product set',
                    lambda mutate=mutate, label=label: publish_product(staged_release(f'blocked-{label}', mutate), blocked, calls))
            require(not calls and not blocked['exists'], 'validation must fail before any Release write')

        # Public verification of the same bytes and metadata.
        def download_from(directory):
            def handler(*args, **kwargs):
                command = list(args[0])
                require(command[:3] == ['gh', 'release', 'download'], 'unexpected external command')
                destination = Path(command[command.index('--dir') + 1])
                for path in directory.iterdir():
                    shutil.copy2(path, destination / path.name)
                return subprocess.CompletedProcess(command, 0)
            return handler

        def verify_published(directory, release_app='product', label='public'):
            public_view['directory'] = directory
            with patch(f'{__name__}.subprocess.run', side_effect=download_from(directory)):
                return verify_public(release_app, tmp / f'download-{label}')

        published = accepts(lambda: verify_published(assembled))
        require(published['assets'] == info['assets'], 'the public manifest matches the assembled manifest')
        accepts(lambda: verify_published(www_dir, 'www', 'portal'))
        public_view['draft'] = True
        rejects('a draft release accepted as public', lambda: verify_published(assembled, label='draft'))
        public_view['draft'] = False
        public_view['source'] = foreign
        rejects('a public tag at another commit', lambda: verify_published(assembled, label='other-commit'))
        public_view['source'] = source
        rejects('a public set with an extra asset',
                lambda: verify_published(staged_release('public-extra', lambda d: (d / 'extra.txt').write_text('x')),
                                         label='extra'))
        rejects('a public set missing payloads',
                lambda: verify_published(staged_release('public-short', lambda d: [
                    p.unlink() for p in d.iterdir() if p.name.startswith('vibeface-core-')]), label='short'))
        rejects('a public set hiding a foreign component under its own manifest',
                lambda: verify_published(staged_release('public-foreign', swap_in_foreign_app), label='foreign-app'),
                'component pin mismatch')
        rejects('a public set with forged APK provenance',
                lambda: verify_published(staged_release('public-apk', forge_apk_provenance), label='forged-apk'),
                'APK provenance mismatch')
        rejects('a public desktop installer built on an unpublished runtime',
                lambda: verify_published(staged_release('public-desktop', forge_desktop_runtime), label='desktop-runtime'),
                'differs from the released CLI bundle')
        rejects('a public manifest with a corrupt checksum',
                lambda: verify_published(staged_release('public-corrupt', lambda d: repoint(
                    d / 'release.json',
                    assets=[dict(a, sha256='0' * 64) if a['name'].endswith('.apk') else a
                            for a in json.loads((d / 'release.json').read_text(encoding='utf-8'))['assets']])), label='corrupt'),
                'checksum mismatch')

        # BUILD-INFO keeps exactly nine fields and one version and commit for CLI, Core and App.
        build = dict(version=product_version, target=TARGETS[0], source_commit=source,
                     update_protocol=control_constant('UPDATE_PROTOCOL'), data_epoch=control_constant('DATA_EPOCH'),
                     core_version=product_version, core_source_commit=source,
                     app_version=product_version, app_source_commit=source)
        accepts(lambda: verify_build_info(build))
        require(len(build) == 9, 'BUILD-INFO keeps its nine fields')
        for change in ({'component_source_commits': {'core': source, 'app': source}}, {'data_epoch': 12},
                       {'data_epoch': '1'}, {'update_protocol': True}, {'core_version': '9.9.9'},
                       {'app_version': '9.9.9'}, {'core_source_commit': foreign}, {'app_source_commit': foreign},
                       {'target': 'mips-unknown'}, {'source_commit': 'z' * 40}):
            rejects(f'build info change {sorted(change)[0]}', lambda change=change: verify_build_info(build | change))
        rejects('build info without the Core version',
                lambda: verify_build_info({k: v for k, v in build.items() if k != 'core_version'}))

    print(f'release self-tests passed: {counts["accepted"]} accepted and {counts["rejected"]} rejected scenarios')
    return counts


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='command', required=True)
    p = sub.add_parser('plan')
    p.add_argument('--app', choices=RELEASES, default='product')
    p.add_argument('--tag')
    p.add_argument('--github-output')
    p.add_argument('--require-main', action='store_true')
    p.add_argument('--publish', action='store_true')
    p = sub.add_parser('component')
    p.add_argument('app', choices=COMPONENTS)
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
    p = sub.add_parser('verify-desktop')
    p.add_argument('--target', required=True, choices=DESKTOP_TARGETS)
    p.add_argument('--runtime-archive', type=Path)
    p.add_argument('directory', type=Path)
    p = sub.add_parser('manifest')
    p.add_argument('app', choices=RELEASES)
    p.add_argument('directory', type=Path)
    p = sub.add_parser('assemble-product')
    p.add_argument('--input-root', type=Path, required=True)
    p.add_argument('--output', type=Path, required=True)
    p = sub.add_parser('publish')
    p.add_argument('app', choices=RELEASES)
    p.add_argument('directory', type=Path)
    sub.add_parser('begin-product')
    p = sub.add_parser('publish-part')
    p.add_argument('--kind', choices=PART_KINDS, required=True)
    p.add_argument('--input', type=Path, required=True)
    p.add_argument('--target')
    p = sub.add_parser('verify-public')
    p.add_argument('app', choices=RELEASES)
    p.add_argument('--output', required=True, type=Path)
    p.add_argument('--repo', default=GITHUB_REPO)
    p = sub.add_parser('mobile-provenance')
    p.add_argument('apk', type=Path)
    sub.add_parser('self-test')
    args = parser.parse_args()
    if args.command == 'plan':
        plan(args.app, tag=args.tag, require_main=args.require_main, publishing=args.publish,
             github_output=args.github_output)
    elif args.command == 'component':
        pack_component(args.app, args.input, args.output, args.target)
    elif args.command == 'stage-native':
        stage_native(args.target, args.input, args.output)
    elif args.command == 'compose':
        compose_offline(args.target, args.native_dir, args.app_archive, args.output)
    elif args.command == 'seal-bundle':
        seal_bundle(args.directory)
    elif args.command == 'verify-desktop':
        product = identity('product')
        pins = {key: product[key] for key in ('version', 'tag', 'source_commit', 'channel')}
        runtime = digest(args.runtime_archive) if args.runtime_archive else None
        print(json.dumps(verify_desktop_receipt(args.directory, args.target, pins, runtime)))
    elif args.command == 'manifest':
        manifest(args.app, args.directory)
    elif args.command == 'assemble-product':
        assemble_product(args.input_root, args.output)
    elif args.command == 'publish':
        publish(args.app, args.directory)
    elif args.command == 'begin-product':
        print(json.dumps(begin_product()))
    elif args.command == 'publish-part':
        print(json.dumps(publish_part(args.kind, args.input, target=args.target)))
    elif args.command == 'verify-public':
        print(json.dumps(verify_public(args.app, args.output, repo=args.repo)))
    elif args.command == 'mobile-provenance':
        write_json(args.apk.with_name(args.apk.name + '.provenance.json'), mobile_provenance(args.apk))
    else:
        self_test()


if __name__ == '__main__':
    main()
