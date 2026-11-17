#!/usr/bin/env python3
"""Independent application release identities, immutable artifacts and dependency pins."""
import argparse
import datetime
import gzip
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath, PureWindowsPath
import plistlib
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import zipfile

sys.dont_write_bytecode = True
if __name__ == '__main__':
    sys.modules.setdefault('release', sys.modules[__name__])

ROOT = Path(__file__).resolve().parent.parent
APPS = RELEASES = ('core', 'app', 'cli', 'desktop', 'mobile', 'www')
COMPONENTS = ('core', 'app', 'www')
TARGETS = ('aarch64-apple-darwin', 'aarch64-unknown-linux-gnu', 'x86_64-unknown-linux-gnu', 'x86_64-pc-windows-msvc')
WINDOWS_TARGETS = ('x86_64-pc-windows-msvc',)
DESKTOP_TARGETS = TARGETS
DESKTOP_INSTALLERS = {'aarch64-apple-darwin': 'dmg', 'x86_64-pc-windows-msvc': 'exe',
                      'x86_64-unknown-linux-gnu': 'AppImage', 'aarch64-unknown-linux-gnu': 'AppImage'}
NATIVE_UI = {'ui': 'gpui', 'gpui_kit': '0.9.1', 'gpui': '0.3.12'}
DESKTOP_SCHEMA = 2
SIGNING_STATES = ('developer-id-notarized', 'authenticode', 'unsigned-development-preview', 'platform-checksums')
VERSION = r'(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-(hourly)\.([0-9]{10})|-(rc)\.([1-9][0-9]*))?'
COMMIT = r'[a-f0-9]{40}'
SHA256 = r'[a-f0-9]{64}'
MAX_ARCHIVE = 384 * 1024 * 1024
MAX_UNPACKED = 512 * 1024 * 1024
SCHEMA_VERSION = 1
GITHUB_REPO = 'leaperone/vibeface'
DEPENDENCIES = {'core': (), 'app': (), 'cli': ('core', 'app'), 'desktop': ('cli',), 'mobile': (), 'www': ()}
WORKFLOWS = {app: f'release-{app}.yml' for app in APPS} | {'cli': 'release.yml', 'desktop': 'release-desktop-app.yml', 'mobile': 'release-mobile.yml'}

RUNTIME_REQUIREMENTS = """# Runtime requirements

The archive contains the CLI, Core, and static browser application. Install the provider tools separately before enabling the bot.

- Codex bots and native App Server access need the `codex` CLI and its authenticated account.
- Cursor uses only the official local Agent CLI (`agent` or `cursor-agent`) and its CLI subscription login. API keys and auth tokens are not required or used, and the official credential store is never overridden.

The device page's automatic connection finds common program installations and completes missing declared authentication variables from the Node environment or a bounded interactive login shell. Existing private settings and enabled uses are preserved. You can still choose an absolute program path in advanced settings.

Codex and Claude keep private session roots and can reuse official CLI access-token snapshots without consuming the source refresh token. Private logins take priority; expired snapshots need a fresh source login or the private login command in settings. Cursor uses private ACP roots with the official CLI authentication store. GUI tokens are not extracted. No credentials or provider runtimes are included.
"""
WINDOWS_RUNTIME_REQUIREMENTS = """
Run `scripts/install-node-service.ps1` from Windows PowerShell to register the per-user service. The
installer needs neither administrator rights, nor WSL, nor Developer Mode.
"""


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
    return ROOT / 'scripts' / ('install-node-service.ps1' if target in WINDOWS_TARGETS else 'install-node-service.sh')


def version(value):
    require(isinstance(value, str), 'invalid release version')
    match = re.fullmatch(VERSION, value)
    require(match, 'invalid release version')
    if match[4]:
        datetime.datetime.strptime(match[5], '%Y%m%d%H')
    return 'hourly' if match[4] else 'rc' if match[6] else 'stable'


def parse_tag(tag):
    match = re.fullmatch(r'(' + '|'.join(APPS) + r')-v(' + VERSION + r')', tag)
    require(match, 'tag must be <core|app|cli|desktop|mobile|www>-v<version>')
    return match[1], match[2], version(match[2])


def read_version(app, root=None):
    root = root or ROOT
    require(app in APPS, 'unknown application')
    directory = root / 'apps' / app
    if app in ('core', 'cli', 'desktop'):
        match = re.search(r'^version\s*=\s*"([^"]+)"', (directory / 'Cargo.toml').read_text(), re.M)
        require(match, 'package must own its version')
        return match[1]
    data = json.loads((directory / ('app.json' if app == 'mobile' else 'package.json')).read_text())
    return data['expo']['extra']['releaseVersion'] if app == 'mobile' else data['version']


def release_tag_for(app, value):
    require(app in APPS, 'unknown application')
    version(value)
    return f'{app}-v{value}'


def identity(app, root=None):
    root = root or ROOT
    value = read_version(app, root)
    return {'app': app, 'version': value, 'tag': release_tag_for(app, value),
            'source_commit': run('git', 'rev-parse', 'HEAD', cwd=root), 'channel': version(value)}


def contracts():
    return {'data_epoch': control_constant('DATA_EPOCH'), 'update_protocol': control_constant('UPDATE_PROTOCOL')}


def validate_pin(app, pin):
    require(isinstance(pin, dict) and set(pin) == {'tag', 'version', 'source_commit', 'manifest_sha256'}, 'invalid dependency pin fields')
    require(parse_tag(pin['tag'])[:2] == (app, pin['version']), 'dependency tag/version mismatch')
    require(isinstance(pin['source_commit'], str) and re.fullmatch(COMMIT, pin['source_commit'])
            and isinstance(pin['manifest_sha256'], str) and re.fullmatch(SHA256, pin['manifest_sha256']), 'invalid dependency identity')
    return pin


def dependencies(app, root=None, required=False):
    require(app in APPS, 'unknown application')
    path = (root or ROOT) / 'releases' / f'{app}.json'
    pins = json.loads(path.read_text()) if path.exists() else {}
    require(isinstance(pins, dict) and set(pins) <= set(DEPENDENCIES[app]), 'unexpected dependency')
    if required:
        require(set(pins) == set(DEPENDENCIES[app]), f'{app} requires exact dependency pins')
    for dependency, pin in pins.items():
        validate_pin(dependency, pin)
    return pins


def component_identity(app, root=None):
    require(app in COMPONENTS, 'unknown component')
    return identity(app, root)


def validate_source(source, app, root=None):
    import hourly
    return hourly.source_context(source, app, root=root or ROOT)


def require_ci(context):
    source = context['ci_source_commit']
    require(isinstance(source, str) and re.fullmatch(COMMIT, source), 'invalid CI source commit')
    batches = json.loads(run('gh', 'api', '--paginate', '--slurp',
        f'repos/{GITHUB_REPO}/actions/workflows/ci.yml/runs?head_sha={source}&branch=main&event=push&per_page=100'))
    for candidate in (item for page in batches for item in page['workflow_runs']):
        if (candidate['head_sha'] == source and candidate['head_branch'] == 'main'
                and candidate['path'] == '.github/workflows/ci.yml' and candidate['event'] == 'push'
                and candidate['repository']['full_name'] == GITHUB_REPO
                and candidate['head_repository']['full_name'] == GITHUB_REPO
                and candidate['status'] == 'completed' and candidate['conclusion'] == 'success'):
            return candidate
    raise ValueError('release requires the exact successful main push CI of its source')


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


def write_runtime_requirements(directory, target):
    text = RUNTIME_REQUIREMENTS
    if target in WINDOWS_TARGETS:
        text += WINDOWS_RUNTIME_REQUIREMENTS
    (Path(directory) / 'RUNTIME-REQUIREMENTS.md').write_text(text, encoding='utf-8', newline='\n')


def public_archive(directory, output):
    def clean(info):
        require(info.isfile() or info.isdir(), 'links and special archive files are forbidden')
        info.uid = info.gid = 0
        info.uname = info.gname = ''
        info.mtime = 0
        info.mode = 0o755 if info.isdir() or info.mode & 0o111 else 0o644
        info.pax_headers = {}
        return info
    output.parent.mkdir(parents=True, exist_ok=True)
    with output.open('wb') as raw:
        with gzip.GzipFile(filename='', fileobj=raw, mode='wb', mtime=0) as compressed:
            with tarfile.open(fileobj=compressed, mode='w') as archive:
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
                          'ui', 'gpui_kit', 'gpui', 'signed', 'signing', 'runtime', 'artifacts'},
            'unsupported desktop receipt fields')
    require(type(info['schema']) is int and info['schema'] == DESKTOP_SCHEMA, 'invalid desktop receipt schema')
    require(info['app'] == 'desktop' and info['target'] == target, 'desktop receipt is not this platform')
    require(all(info[key] == value for key, value in pins.items()), 'desktop receipt cohort mismatch')
    require(re.fullmatch(COMMIT, info['source_commit']), 'invalid desktop source commit')
    require(all(info.get(key) == value for key, value in NATIVE_UI.items()),
            'desktop receipt must name the pinned native GPUI version')
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
    require(set(info['runtime']) == {'version', 'source_commit', 'archive', 'sha256'}, 'unsupported desktop runtime reference')
    require(info['runtime']['archive'] == f'vibeface-{info["runtime"]["version"]}-{target}.tar.gz',
            'desktop runtime must be the pinned platform CLI bundle')
    require(isinstance(info['runtime']['sha256'], str) and re.fullmatch(r'[a-f0-9]{64}', info['runtime']['sha256']),
            'invalid desktop runtime digest')
    if runtime_digest:
        require(info['runtime']['sha256'] == runtime_digest, 'desktop runtime differs from the released CLI bundle')
    version(info['runtime']['version'])
    require(re.fullmatch(COMMIT, info['runtime']['source_commit']), 'invalid desktop runtime source')
    pins_lock = dependencies('desktop')
    if pins_lock:
        pin = pins_lock['cli']
        require(info['runtime']['version'] == pin['version'] and info['runtime']['source_commit'] == pin['source_commit'],
                'desktop runtime differs from its exact dependency pin')
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



def verify_component(directory, app, expected=None):
    directory = Path(directory)
    info = json.loads((directory / 'COMPONENT-INFO.json').read_text(encoding='utf-8'))
    fields = {'app', 'version', 'tag', 'source_commit', 'channel', 'schema', 'target', 'files'}
    if app == 'core':
        fields.add('data_epoch')
    require(set(info) == fields and info['app'] == app and type(info['schema']) is int
            and info['schema'] == SCHEMA_VERSION, 'unsupported component identity')
    require(parse_tag(info['tag']) == (app, info['version'], info['channel']), 'invalid component tag')
    require(isinstance(info['source_commit'], str) and re.fullmatch(COMMIT, info['source_commit']), 'invalid component source')
    if app == 'core':
        require(type(info['data_epoch']) is int and info['data_epoch'] == control_constant('DATA_EPOCH')
                and info['target'] in TARGETS, 'unsupported Core data epoch or target')
        require(set(info['files']) == {program_names(info['target'])[1]}, 'Core component must contain only its binary')
    else:
        prefix = 'web/' if app == 'app' else 'site/'
        require(app in ('app', 'www') and info['target'] is None
                and all(name.startswith(prefix) for name in info['files']), 'invalid static component layout')
    if expected:
        require(all(info[k] == value for k, value in expected.items()), 'component pin mismatch')
    require(info['files'] and info['files'] == files(directory, ('COMPONENT-INFO.json',)), 'component content changed')
    return info


def pack_component(app, source, output, target=None):
    source, output = Path(source), Path(output)
    info = component_identity(app)
    require(not run('git', 'status', '--porcelain'), 'release source must be clean')
    if app == 'core':
        require(target in TARGETS and source.is_file(), 'Core requires a target binary')
        require(source.name == program_names(target)[1], 'Core binary name must match its platform')
        require(run(str(source.resolve()), '--version') == 'vibeface-core ' + info['version'], 'Core binary version mismatch')
    else:
        require(source.is_dir() and target is None, 'static component requires a directory')
    name = f'vibeface-{app}-{info["version"]}' + (f'-{target}' if target else '')
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=output, prefix='.component-') as tmp:
        directory = Path(tmp) / name
        directory.mkdir()
        if app == 'core':
            shutil.copy2(source, directory / program_names(target)[1])
        else:
            shutil.copytree(source, directory / ('web' if app == 'app' else 'site'))
        payload = dict(info, schema=SCHEMA_VERSION, target=target, files=files(directory))
        if app == 'core':
            payload['data_epoch'] = control_constant('DATA_EPOCH')
        write_json(directory / 'COMPONENT-INFO.json', payload)
        verify_component(directory, app)
        public_archive(directory, output / f'{name}.tar.gz')
    manifest(app, output)


def verify_build_info(info):
    require(set(info) == {'version', 'target', 'source_commit', 'update_protocol', 'data_epoch',
                          'core_version', 'core_source_commit', 'app_version', 'app_source_commit'}, 'unsupported build info fields')
    require(info['target'] in TARGETS, 'unsupported build target')
    for key, expected in contracts().items():
        require(type(info[key]) is int and info[key] == expected, 'unsupported build protocol or data epoch')
    for key in ('source_commit', 'core_source_commit', 'app_source_commit'):
        require(isinstance(info[key], str) and re.fullmatch(COMMIT, info[key]), 'invalid build source')
    for key in ('version', 'core_version', 'app_version'):
        version(info[key])
    return info


def seal_bundle(directory, core_info=None, app_info=None):
    directory = Path(directory)
    require(not run('git', 'status', '--porcelain'), 'package source must be clean')
    source = run('git', 'rev-parse', 'HEAD')
    core_info = core_info or {'version': read_version('core'), 'source_commit': source}
    app_info = app_info or {'version': read_version('app'), 'source_commit': source}
    build = dict(version=read_version('cli'), target=os.environ['PACKAGE_TARGET'], source_commit=source,
                 **contracts(), core_version=core_info['version'], core_source_commit=core_info['source_commit'],
                 app_version=app_info['version'], app_source_commit=app_info['source_commit'])
    verify_build_info(build)
    write_json(directory / 'BUILD-INFO.json', build)
    (directory / 'FILES.sha256').write_text(''.join(f'{value}  {name}\n' for name, value in files(directory, ('FILES.sha256',)).items()),
                                           encoding='utf-8', newline='\n')
    return build


def native_receipt(native_dir, target, cli_version, source_commit):
    info = json.loads((Path(native_dir) / 'NATIVE-INFO.json').read_text())
    require(set(info) == {'schema', 'target', 'version', 'source_commit', 'files', 'data_epoch'}, 'unsupported native receipt')
    require(info['schema'] == SCHEMA_VERSION and info['target'] == target and info['version'] == cli_version
            and info['source_commit'] == source_commit and info['data_epoch'] == control_constant('DATA_EPOCH'), 'native staging identity mismatch')
    require(set(info['files']) == {program_names(target)[0]}
            and files(Path(native_dir), ('NATIVE-INFO.json',)) == info['files'], 'CLI native staging bytes differ')
    return info


def stage_native(target, binary_directory, output, app='cli'):
    require(app == 'cli' and target in TARGETS, 'native staging owns CLI only')
    info = identity('cli')
    binary = Path(binary_directory) / program_names(target)[0]
    require(binary.is_file() and not binary.is_symlink(), 'CLI binary is missing')
    require(run(str(binary.resolve()), '--version') == 'vibeface ' + info['version'], 'CLI binary version mismatch')
    require(not run('git', 'status', '--porcelain'), 'release source must be clean')
    output = Path(output)
    output.mkdir(parents=True, exist_ok=True)
    require(not any(output.iterdir()), 'native output must be empty')
    shutil.copy2(binary, output / binary.name)
    write_json(output / 'NATIVE-INFO.json', dict(schema=SCHEMA_VERSION, target=target, version=info['version'],
               source_commit=info['source_commit'], files=files(output), data_epoch=control_constant('DATA_EPOCH')))
    return native_receipt(output, target, info['version'], info['source_commit'])


def compose_offline(target, native_dir, app_archive, output, core_archive):
    require(target in TARGETS, 'unsupported platform')
    info = identity('cli')
    require(not run('git', 'status', '--porcelain'), 'release source must be clean')
    native_dir, output = Path(native_dir), Path(output)
    native_receipt(native_dir, target, info['version'], info['source_commit'])
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=output, prefix='.compose-') as temporary:
        tmp = Path(temporary)
        app_root = unpack(Path(app_archive), tmp / 'app')
        core_root = unpack(Path(core_archive), tmp / 'core')
        app_info = verify_component(app_root, 'app')
        core_info = verify_component(core_root, 'core', {'target': target})
        pins = dependencies('cli')
        if pins:
            for app, component in (('core', core_info), ('app', app_info)):
                require(component['tag'] == pins[app]['tag'] and component['source_commit'] == pins[app]['source_commit'], 'bundle dependency pin mismatch')
        name = f'vibeface-{info["version"]}-{target}'
        directory = tmp / name
        directory.mkdir()
        cli, core = program_names(target)
        shutil.copy2(native_dir / cli, directory / cli)
        shutil.copy2(core_root / core, directory / core)
        shutil.copytree(app_root / 'web', directory / 'web')
        (directory / 'scripts').mkdir()
        installer = service_installer(target)
        shutil.copy2(installer, directory / 'scripts' / installer.name)
        import importlib.util
        spec = importlib.util.spec_from_file_location('offline_docs', ROOT / 'scripts/offline-docs.py')
        docs = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(docs)
        docs.assemble(directory)
        write_runtime_requirements(directory, target)
        os.environ['PACKAGE_TARGET'] = target
        seal_bundle(directory, core_info, app_info)
        public_archive(directory, output / (name + '.tar.gz'))
    manifest('cli', output)


def verify_checksum_file(directory, name):
    path = Path(directory) / name
    require(path.is_file() and not path.is_symlink(), 'missing release payload')
    checksum = path.with_name(path.name + '.sha256')
    require(checksum.is_file() and checksum.read_text(encoding='utf-8') == f'{digest(path)}  {name}\n', 'release checksum mismatch')


def expected_asset_names(app, value, ios_signed=False):
    require(app in APPS, 'unknown application')
    if app in ('app', 'www'):
        payload = {f'vibeface-{app}-{value}.tar.gz'}
    elif app in ('core', 'cli'):
        prefix = 'vibeface-core' if app == 'core' else 'vibeface'
        payload = {f'{prefix}-{value}-{target}.tar.gz' for target in TARGETS}
    elif app == 'desktop':
        return {'release.json'} | set().union(*(desktop_asset_names(value, target) for target in TARGETS))
    else:
        return {'release.json'} | apk_asset_names(value) | ios_asset_names(value, ios_signed)
    return {'release.json'} | payload | {name + '.sha256' for name in payload}


def verify_release_identity(info):
    require(isinstance(info, dict) and set(info) == {'schema', 'app', 'version', 'tag', 'source_commit', 'channel', 'contracts', 'dependencies', 'assets'},
            'unsupported release manifest fields')
    require(type(info['schema']) is int and info['schema'] == SCHEMA_VERSION and info['app'] in APPS, 'invalid release manifest schema/application')
    require(parse_tag(info['tag']) == (info['app'], info['version'], info['channel']), 'invalid release identity')
    require(isinstance(info['source_commit'], str) and re.fullmatch(COMMIT, info['source_commit']), 'invalid release source')
    require(info['contracts'] == contracts() and all(type(v) is int for v in info['contracts'].values()), 'release contract mismatch')
    require(isinstance(info['dependencies'], dict) and set(info['dependencies']) == set(DEPENDENCIES[info['app']]), 'release dependency set mismatch')
    for dependency, pin in info['dependencies'].items():
        validate_pin(dependency, pin)
        require(info['channel'] != 'stable' or parse_tag(pin['tag'])[2] == 'stable', 'stable releases require stable dependency pins')
    names = set()
    require(isinstance(info['assets'], list) and info['assets'], 'release contains no assets')
    for asset in info['assets']:
        require(isinstance(asset, dict) and set(asset) == {'name', 'size', 'sha256'}, 'invalid release asset fields')
        name = asset['name']
        safe_name(name)
        require('/' not in name and name != 'release.json' and name not in names, 'duplicate or invalid release asset name')
        require(type(asset['size']) is int and asset['size'] > 0 and isinstance(asset['sha256'], str)
                and re.fullmatch(SHA256, asset['sha256']), 'invalid release asset metadata')
        names.add(name)
    return info


def manifest(app, output):
    output = Path(output)
    info = dict(identity(app), schema=SCHEMA_VERSION, contracts=contracts(), dependencies=dependencies(app, required=True),
                assets=[{'name': p.name, 'size': p.stat().st_size, 'sha256': digest(p)}
                        for p in sorted(output.iterdir()) if p.is_file() and p.name != 'release.json'])
    verify_release_identity(info)
    write_json(output / 'release.json', info)
    return info


def verify_manifest(directory, expected=None):
    directory = Path(directory)
    info = verify_release_identity(json.loads((directory / 'release.json').read_text()))
    if expected:
        require(all(info[k] == value for k, value in expected.items()), 'release manifest pin mismatch')
    names = {asset['name'] for asset in info['assets']}
    require(names == {p.name for p in directory.iterdir()} - {'release.json'}, 'unexpected release files')
    for asset in info['assets']:
        path = directory / asset['name']
        require(path.is_file() and not path.is_symlink() and path.stat().st_size == asset['size']
                and digest(path) == asset['sha256'], 'release asset checksum mismatch')
    return info


def verify_bundle(directory, target, info):
    build = verify_build_info(json.loads((directory / 'BUILD-INFO.json').read_text()))
    require(build['target'] == target and build['version'] == info['version'] and build['source_commit'] == info['source_commit'], 'CLI bundle identity mismatch')
    for dependency, pin in info['dependencies'].items():
        require(build[dependency + '_version'] == pin['version'] and build[dependency + '_source_commit'] == pin['source_commit'], 'CLI bundle dependency differs from manifest')
    declared = (directory / 'FILES.sha256').read_text()
    expected = ''.join(f'{value}  {name}\n' for name, value in files(directory, ('FILES.sha256',)).items())
    require(declared == expected, 'CLI sealed file checksums differ')
    return build


def verify_release(app, directory, expected=None):
    directory = Path(directory)
    info = verify_manifest(directory, expected)
    require(info['app'] == app, 'release application differs')
    signed = app == 'mobile' and (directory / f'vibeface-{info["version"]}-ios.ipa').is_file()
    require({p.name for p in directory.iterdir()} == expected_asset_names(app, info['version'], signed), 'incomplete release asset set')
    pins = {key: info[key] for key in ('version', 'tag', 'source_commit', 'channel')}
    for asset in info['assets']:
        name = asset['name']
        if name + '.sha256' in {p.name for p in directory.iterdir()}:
            verify_checksum_file(directory, name)
        if name.endswith('.tar.gz'):
            with tempfile.TemporaryDirectory() as temp:
                unpacked = unpack(directory / name, Path(temp) / 'unpack')
                if app == 'cli':
                    target = next(target for target in TARGETS if name.endswith(target + '.tar.gz'))
                    verify_bundle(unpacked, target, info)
                elif app in COMPONENTS:
                    verify_component(unpacked, app, pins)
    if app == 'desktop':
        for target in TARGETS:
            verify_desktop_receipt(directory, target, pins)
    if app == 'mobile':
        name = f'vibeface-{info["version"]}-android.apk'
        receipt = json.loads((directory / (name + '.provenance.json')).read_text())
        require(receipt == mobile_provenance(directory / name), 'Android provenance differs')
        verify_ios_assets(directory, pins)
    return info


def collect(app, input_root, output):
    input_root, output = Path(input_root), Path(output)
    expected = identity(app)
    output.mkdir(parents=True, exist_ok=True)
    require(not any(output.iterdir()), 'collection output must be empty')
    for source in sorted(input_root.rglob('release.json')):
        part = verify_manifest(source.parent, expected)
        require(part['dependencies'] == dependencies(app, required=True), 'producer dependency pins differ')
        for asset in part['assets']:
            path = output / asset['name']
            require(not path.exists(), 'producers must own distinct asset names')
            shutil.copy2(source.parent / path.name, path)
    manifest(app, output)
    return verify_release(app, output, expected)


def plan(app, tag=None, require_main=False, publishing=False, github_output=None):
    info = identity(app)
    if tag:
        require(parse_tag(tag) == (app, info['version'], info['channel']), 'tag does not match selected application/version')
    if publishing or require_main:
        require(not run('git', 'status', '--porcelain'), 'release source must be clean')
        info.update(validate_source(info['source_commit'], app))
        require_ci(info)
        dependencies(app, required=True)
    if publishing:
        require(run('git', 'rev-parse', info['tag'] + '^{commit}') == info['source_commit'], 'publishing requires exact existing tag')
    if github_output:
        with open(github_output, 'a') as output:
            for key, value in info.items():
                output.write(f'{key}={value}\n')
    print(json.dumps(info))
    return info

def github_json(path):
    return json.loads(run('gh', 'api', f'repos/{GITHUB_REPO}/{path}'))


def fetch_source(tag):
    args = ['git', 'fetch', '--quiet', '--tags']
    if run('git', 'rev-parse', '--is-shallow-repository') == 'true':
        args.append('--unshallow')
    subprocess.run([*args, 'origin', 'main', 'refs/tags/' + tag], cwd=ROOT, check=True)
    return run('git', 'rev-parse', tag + '^{commit}')


def published(tag, pin=None, prove_source=True):
    app, value, channel = parse_tag(tag)
    record = github_json('releases/tags/' + tag)
    require(record['tag_name'] == tag and record['draft'] is False
            and record['prerelease'] == (channel != 'stable'), 'release publication identity differs')
    by_name = {asset['name']: asset for asset in record['assets']}
    require(len(by_name) == len(record['assets']) and 'release.json' in by_name, 'published complete manifest is required')
    marker = by_name['release.json']
    require(type(marker['size']) is int and 0 < marker['size'] <= 1024 * 1024, 'manifest exceeds limit')
    raw = subprocess.check_output(['gh', 'api', '-H', 'Accept: application/octet-stream',
                                  f'repos/{GITHUB_REPO}/releases/assets/{marker["id"]}'], cwd=ROOT)
    sha = hashlib.sha256(raw).hexdigest()
    require(len(raw) == marker['size'] and marker.get('digest') == 'sha256:' + sha, 'published manifest digest differs')
    info = verify_release_identity(json.loads(raw))
    require((info['app'], info['version'], info['channel'], info['tag']) == (app, value, channel, tag), 'published manifest tag differs')
    expected = {asset['name']: asset for asset in info['assets']}
    require(set(by_name) == set(expected) | {'release.json'}, 'release contains unexpected assets')
    signed = app == 'mobile' and f'vibeface-{value}-ios.ipa' in expected
    require(set(by_name) == expected_asset_names(app, value, signed), 'release is incomplete')
    for name, asset in expected.items():
        require(by_name[name]['size'] == asset['size'] and by_name[name].get('digest') == 'sha256:' + asset['sha256'], 'published asset metadata differs')
    if pin:
        validate_pin(app, pin)
        require(pin == {key: info[key] for key in ('tag', 'version', 'source_commit')} | {'manifest_sha256': sha}, 'published dependency pin differs')
    if prove_source:
        source = fetch_source(tag)
        require(source == info['source_commit'], 'published source differs from immutable tag')
        require_ci(validate_source(source, app))
    return info, record, sha


def download_assets(tag, selected, output):
    output = Path(output)
    output.mkdir(parents=True, exist_ok=True)
    for asset in selected:
        destination = output / asset['name']
        if not destination.exists():
            subprocess.run(['gh', 'release', 'download', tag, '--repo', GITHUB_REPO, '--pattern', asset['name'], '--dir', str(output)], check=True)
        require(destination.is_file() and not destination.is_symlink() and destination.stat().st_size == asset['size']
                and digest(destination) == asset['sha256'], 'downloaded dependency bytes differ')
    return output


def dependency(app, dependency_app, target, output):
    pins = dependencies(app, required=True)
    require(dependency_app in pins, 'application does not own this dependency')
    pin = pins[dependency_app]
    info, _, _ = published(pin['tag'], pin)
    prefix = 'vibeface' if dependency_app == 'cli' else 'vibeface-' + dependency_app
    suffix = '-' + target if dependency_app in ('core', 'cli') else ''
    require(dependency_app not in ('core', 'cli') or target in TARGETS, 'native dependency requires a target')
    name = f'{prefix}-{pin["version"]}{suffix}.tar.gz'
    wanted = {name, name + '.sha256'}
    selected = [item for item in info['assets'] if item['name'] in wanted]
    require(len(selected) == 2, 'dependency lacks target payload/checksum')
    download_assets(pin['tag'], selected, output)
    verify_checksum_file(Path(output), name)
    return info


def publish(app, directory, repo=GITHUB_REPO):
    require(repo == GITHUB_REPO, 'unexpected publication repository')
    expected = identity(app)
    plan(app, expected['tag'], require_main=True, publishing=True)
    info = verify_release(app, Path(directory), expected)
    tag = info['tag']
    view = subprocess.run(['gh', 'release', 'view', tag, '--repo', repo, '--json', 'isDraft,assets'], capture_output=True, text=True)
    if view.returncode:
        require('release not found' in view.stderr.lower(), 'cannot determine release state; reconcile before publishing')
        subprocess.run(['gh', 'release', 'create', tag, '--repo', repo, '--verify-tag', '--title', tag,
                        '--notes', 'BUILDING; incomplete until release.json is published.', '--latest=false',
                        '--prerelease=' + str(info['channel'] != 'stable').lower()], check=True)
        remote = {}
    else:
        record = json.loads(view.stdout)
        require(record['isDraft'] is False, 'release is still a draft')
        remote = {asset['name']: asset for asset in record['assets']}
        require(len(remote) == len(record['assets']), 'duplicate remote release assets')
    paths = [Path(directory) / asset['name'] for asset in info['assets']] + [Path(directory) / 'release.json']
    require(set(remote) <= {p.name for p in paths}, 'release has foreign assets')
    with tempfile.TemporaryDirectory() as temp:
        for path in paths:
            if path.name in remote:
                asset = remote[path.name]
                if asset.get('digest') is not None:
                    require(asset['size'] == path.stat().st_size and asset['digest'] == 'sha256:' + digest(path), 'immutable published bytes differ; use a new version')
                else:
                    downloaded = download_assets(tag, [{'name': path.name, 'size': path.stat().st_size, 'sha256': digest(path)}], Path(temp))
                    require(digest(downloaded / path.name) == digest(path), 'immutable release asset differs')
                continue
            # A write with an unknown result is never retried. The next invocation reconciles all uploaded bytes.
            subprocess.run(['gh', 'release', 'upload', tag, '--repo', repo, str(path)], check=True)
    subprocess.run(['gh', 'release', 'edit', tag, '--repo', repo, '--notes',
                    f'{app} {info["version"]}; source {info["source_commit"]}.', '--latest=false'], check=True)
    return info


def verify_public(app, destination, repo=GITHUB_REPO):
    info, _, _ = published(identity(app)['tag'])
    require(info['app'] == app and repo == GITHUB_REPO, 'wrong public application')
    download_assets(info['tag'], info['assets'], destination)
    write_json(Path(destination) / 'release.json', info)
    return verify_release(app, Path(destination), identity(app))


def self_test():
    from unittest.mock import patch
    import copy
    for app in APPS:
        assert parse_tag(f'{app}-v0.0.1') == (app, '0.0.1', 'stable')
        assert parse_tag(f'{app}-v0.0.1-hourly.2026111409') == (app, '0.0.1-hourly.2026111409', 'hourly')
    def rejects(action):
        try:
            action()
        except (ValueError, KeyError, TypeError):
            return
        raise AssertionError('invalid release was accepted')
    for tag in ('v0.0.1', 'product-v0.0.1', 'core-v1.0.0-nightly.1', 'cli-v1.0.0-hourly.2026111424'):
        rejects(lambda: parse_tag(tag))
    build = dict(version='0.0.1-hourly.2026111409', target='x86_64-unknown-linux-gnu', source_commit='a' * 40,
                 core_version='0.0.1-hourly.2026111408', core_source_commit='b' * 40,
                 app_version='0.0.1-rc.2', app_source_commit='c' * 40, **contracts())
    assert verify_build_info(build) == build
    rejects(lambda: verify_build_info(build | {'data_epoch': 2}))
    rejects(lambda: verify_build_info(build | {'core_source_commit': 'invalid'}))
    def pin(app, version_value, source):
        return {'tag': f'{app}-v{version_value}', 'version': version_value, 'source_commit': source, 'manifest_sha256': 'd' * 64}
    pins = {'core': pin('core', build['core_version'], 'b' * 40), 'app': pin('app', build['app_version'], 'c' * 40)}
    own = dict(app='cli', tag='cli-v' + build['version'], version=build['version'], source_commit='a' * 40, channel='hourly')
    with tempfile.TemporaryDirectory() as temporary:
        tmp = Path(temporary)
        package = tmp / 'package'
        package.mkdir()
        write_json(package / 'BUILD-INFO.json', build)
        (package / 'vibeface').write_text('CLI bytes')
        (package / 'vibeface-core').write_text('Core bytes')
        (package / 'FILES.sha256').write_text(''.join(f'{value}  {name}\n' for name, value in files(package).items()))
        info = own | {'schema': 1, 'contracts': contracts(), 'dependencies': pins,
                      'assets': [{'name': 'fixture.tar.gz', 'size': 1, 'sha256': 'd' * 64}]}
        assert verify_release_identity(info) == info
        assert verify_bundle(package, build['target'], info) == build
        bad = copy.deepcopy(info)
        bad['dependencies']['core']['version'] = '1.0.0'
        rejects(lambda: verify_bundle(package, build['target'], bad))
        (package / 'vibeface-core').write_text('tampered bytes')
        rejects(lambda: verify_bundle(package, build['target'], info))
        bad = copy.deepcopy(info)
        bad['assets'].append(bad['assets'][0])
        rejects(lambda: verify_release_identity(bad))
        bad = copy.deepcopy(info)
        bad['dependencies']['core']['manifest_sha256'] = 'invalid'
        rejects(lambda: verify_release_identity(bad))
        archive = tmp / 'a.tar.gz'
        public_archive(package, archive)
        assert (unpack(archive, tmp / 'unpacked') / 'vibeface').read_text() == 'CLI bytes'
        for app in ('core', 'app', 'www'):
            base = dict(app=app, version='0.0.1', tag=app + '-v0.0.1', source_commit='a' * 40, channel='stable')
            out = tmp / app
            out.mkdir()
            (out / 'fixture').write_text('fixture')
            with patch(__name__ + '.identity', return_value=base), patch(__name__ + '.dependencies', return_value={}):
                manifest(app, out)
                assert verify_manifest(out)['app'] == app
                (out / 'fixture').write_text('changed')
                rejects(lambda: verify_manifest(out))
    print('Independent release self-test passed: six tags, independent pins, sealed bytes and tampering rejection')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='command', required=True)
    p = sub.add_parser('plan')
    p.add_argument('--app', choices=APPS, required=True)
    p.add_argument('--tag')
    p.add_argument('--require-main', action='store_true')
    p.add_argument('--publish', action='store_true')
    p.add_argument('--github-output')
    p = sub.add_parser('component')
    p.add_argument('app', choices=COMPONENTS)
    p.add_argument('--input', type=Path, required=True)
    p.add_argument('--output', type=Path, required=True)
    p.add_argument('--target')
    p = sub.add_parser('stage-native')
    p.add_argument('--app', choices=('cli',), default='cli')
    p.add_argument('--target', required=True)
    p.add_argument('--input', type=Path, required=True)
    p.add_argument('--output', type=Path, required=True)
    p = sub.add_parser('compose')
    p.add_argument('--target', required=True)
    p.add_argument('--native-dir', type=Path, required=True)
    p.add_argument('--app-archive', type=Path, required=True)
    p.add_argument('--core-archive', type=Path, required=True)
    p.add_argument('--output', type=Path, required=True)
    sub.add_parser('seal-bundle').add_argument('directory', type=Path)
    p = sub.add_parser('manifest')
    p.add_argument('app', choices=APPS)
    p.add_argument('directory', type=Path)
    for command in ('publish', 'verify-public'):
        p = sub.add_parser(command)
        p.add_argument('app', choices=APPS)
        p.add_argument('directory', type=Path)
    p = sub.add_parser('collect')
    p.add_argument('--app', choices=APPS, required=True)
    p.add_argument('--input-root', type=Path, required=True)
    p.add_argument('--output', type=Path, required=True)
    p = sub.add_parser('dependency')
    p.add_argument('--app', choices=APPS, required=True)
    p.add_argument('--dependency', choices=APPS, required=True)
    p.add_argument('--target')
    p.add_argument('--output', type=Path, required=True)
    p = sub.add_parser('verify-desktop')
    p.add_argument('--target', required=True)
    p.add_argument('--runtime-archive', type=Path)
    p.add_argument('directory', type=Path)
    sub.add_parser('mobile-provenance').add_argument('apk', type=Path)
    sub.add_parser('self-test')
    args = parser.parse_args()
    if args.command == 'plan':
        plan(args.app, args.tag, args.require_main, args.publish, args.github_output)
    elif args.command == 'component':
        pack_component(args.app, args.input, args.output, args.target)
    elif args.command == 'stage-native':
        stage_native(args.target, args.input, args.output, args.app)
    elif args.command == 'compose':
        compose_offline(args.target, args.native_dir, args.app_archive, args.output, args.core_archive)
    elif args.command == 'seal-bundle':
        seal_bundle(args.directory)
    elif args.command == 'manifest':
        manifest(args.app, args.directory)
    elif args.command == 'collect':
        collect(args.app, args.input_root, args.output)
    elif args.command == 'dependency':
        dependency(args.app, args.dependency, args.target, args.output)
    elif args.command == 'publish':
        publish(args.app, args.directory)
    elif args.command == 'verify-public':
        verify_public(args.app, args.directory)
    elif args.command == 'mobile-provenance':
        write_json(args.apk.with_name(args.apk.name + '.provenance.json'), mobile_provenance(args.apk))
    elif args.command == 'verify-desktop':
        pins = {key: value for key, value in identity('desktop').items() if key != 'app'}
        verify_desktop_receipt(args.directory, args.target, pins, digest(args.runtime_archive) if args.runtime_archive else None)
    else:
        self_test()


if __name__ == '__main__':
    main()
