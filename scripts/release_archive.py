"""Deterministic archive bytes, sealed bundles and platform provenance receipts."""
import gzip
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import plistlib
import re
import shutil
import tarfile
import zipfile
import release_context
import release_format


MAX_ARCHIVE = 384 * 1024 * 1024


MAX_UNPACKED = 512 * 1024 * 1024


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


def digest(path):
    h = hashlib.sha256()
    with path.open('rb') as f:
        for block in iter(lambda: f.read(1024 * 1024), b''):
            h.update(block)
    return h.hexdigest()


def files(directory, exclude=()):
    result = {}
    for path in sorted(directory.rglob('*'), key=lambda path: path.relative_to(directory).as_posix()):
        release_format.require(not path.is_symlink(), 'symlinks are not allowed in release assets')
        if path.is_file():
            name = path.relative_to(directory).as_posix()
            release_format.safe_name(name)
            if name not in exclude:
                result[name] = digest(path)
    release_format.require(len(result) <= 4096, 'too many release files')
    return result


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + '\n', encoding='utf-8', newline='\n')


def write_runtime_requirements(directory, target):
    text = RUNTIME_REQUIREMENTS
    if target in release_format.WINDOWS_TARGETS:
        text += WINDOWS_RUNTIME_REQUIREMENTS
    (Path(directory) / 'RUNTIME-REQUIREMENTS.md').write_text(text, encoding='utf-8', newline='\n')


def public_archive(directory, output):
    def clean(info):
        release_format.require(info.isfile() or info.isdir(), 'links and special archive files are forbidden')
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
    release_format.require(archive.stat().st_size <= MAX_ARCHIVE, 'archive exceeds size limit')
    with gzip.open(archive, 'rb') as compressed:
        raw = compressed.read(MAX_UNPACKED + 1)
    release_format.require(len(raw) <= MAX_UNPACKED, 'unpacked archive exceeds limit')
    with tarfile.open(fileobj=io.BytesIO(raw), mode='r:') as source:
        members = source.getmembers()
        release_format.require(0 < len(members) <= 4096, 'invalid archive entry count')
        release_format.require(sum(x.size for x in members) <= MAX_UNPACKED, 'unpacked archive exceeds limit')
        seen = set()
        roots = set()
        for item in members:
            name = item.name.rstrip('/')
            release_format.safe_name(name)
            release_format.require(name not in seen and (item.isfile() or item.isdir()), 'duplicate or special archive entry')
            roots.add(name.split('/')[0]); seen.add(name)
        release_format.require(len(roots) == 1, 'archive must contain one root')
        for item in members:
            item.mode &= 0o777
        source.extractall(destination, members=members)
    return destination / roots.pop()


def mobile_provenance(apk):
    expo = json.loads((release_context.ROOT / 'apps/mobile/app.json').read_text(encoding='utf-8'))['expo']
    candidates = sorted((Path(os.environ.get('ANDROID_HOME', '/nonexistent')) / 'build-tools').glob('*/aapt'))
    tool = shutil.which('aapt') or (str(candidates[-1]) if candidates else None)
    release_format.require(tool, 'Android aapt is required to verify APK identity')
    badging = release_context.run(tool, 'dump', 'badging', str(apk.resolve()))
    match = re.search(r"^package: name='([^']+)' versionCode='([0-9]+)' versionName='([^']+)'", badging, re.M)
    release_format.require(match, 'APK has no Android package identity')
    package, code, marketing = match.groups()
    release_format.require((package, int(code), marketing) == (expo['android']['package'], expo['android']['versionCode'], expo['version']),
            'APK package, versionCode or marketing version mismatch')
    return dict(version=release_context.read_version('mobile'), sourceCommit=release_context.run('git', 'rev-parse', 'HEAD'), sha256=digest(apk),
                applicationId=package, versionCode=int(code), versionName=marketing)


def verify_desktop_receipt(directory, target, pins, runtime_digest=None):
    """Read a desktop provenance receipt and bind it to its payloads, cohort and bundled runtime."""
    release_format.require(target in release_format.TARGETS, 'unsupported desktop target')
    version_value = pins['version']
    path = directory / f'{release_format.desktop_basename(version_value, target)}.provenance.json'
    release_format.require(path.is_file(), 'missing desktop provenance receipt')
    info = json.loads(path.read_text(encoding='utf-8'))
    release_format.require(set(info) == {'schema', 'app', 'target', 'version', 'tag', 'channel', 'source_commit',
                          'ui', 'gpui_kit', 'gpui', 'signed', 'signing', 'runtime', 'artifacts'},
            'unsupported desktop receipt fields')
    release_format.require(type(info['schema']) is int and info['schema'] == release_format.DESKTOP_SCHEMA, 'invalid desktop receipt schema')
    release_format.require(info['app'] == 'desktop' and info['target'] == target, 'desktop receipt is not this platform')
    release_format.require(all(info[key] == value for key, value in pins.items()), 'desktop receipt cohort mismatch')
    release_format.require(re.fullmatch(release_format.COMMIT, info['source_commit']), 'invalid desktop source commit')
    release_format.require(all(info.get(key) == value for key, value in release_format.NATIVE_UI.items()),
            'desktop receipt must name the pinned native GPUI version')
    release_format.require(type(info['signed']) is bool and info['signing'] in release_format.SIGNING_STATES, 'invalid desktop signing status')
    release_format.require(info['signed'] == (info['signing'] in ('developer-id-notarized', 'authenticode')),
            'desktop signing status and signed flag disagree')
    linux = target.endswith('-unknown-linux-gnu')
    release_format.require((info['signing'] == 'platform-checksums') == linux,
            'Linux platform checksums must match the desktop target')
    release_format.require(info['signed'] or linux or pins['channel'] != 'stable',
            'stable desktop distribution requires a signed application')
    expected_signing = 'developer-id-notarized' if target == 'aarch64-apple-darwin' else 'authenticode'
    release_format.require(not info['signed'] or info['signing'] == expected_signing, 'desktop signing kind does not match its platform')
    release_format.require(set(info['runtime']) == {'version', 'source_commit', 'archive', 'sha256'}, 'unsupported desktop runtime reference')
    release_format.require(info['runtime']['archive'] == f'vibeface-{info["runtime"]["version"]}-{target}.tar.gz',
            'desktop runtime must be the pinned platform CLI bundle')
    release_format.require(isinstance(info['runtime']['sha256'], str) and re.fullmatch(r'[a-f0-9]{64}', info['runtime']['sha256']),
            'invalid desktop runtime digest')
    if runtime_digest:
        release_format.require(info['runtime']['sha256'] == runtime_digest, 'desktop runtime differs from the released CLI bundle')
    release_format.version(info['runtime']['version'])
    release_format.require(re.fullmatch(release_format.COMMIT, info['runtime']['source_commit']), 'invalid desktop runtime source')
    pins_lock = release_context.dependencies('desktop')
    if pins_lock:
        pin = pins_lock['cli']
        release_format.require(info['runtime']['version'] == pin['version'] and info['runtime']['source_commit'] == pin['source_commit'],
                'desktop runtime differs from its exact dependency pin')
    payloads = release_format.desktop_payload_names(version_value, target)
    release_format.require(set(info['artifacts']) == set(payloads), 'desktop receipt must cover exactly its two payloads')
    for name in payloads:
        payload = directory / name
        release_format.require(payload.is_file() and not payload.is_symlink(), 'missing regular desktop payload')
        release_format.require(digest(payload) == info['artifacts'][name], 'desktop payload digest mismatch')
        verify_checksum_file(directory, name)
    return info


def verify_ios_assets(directory, pins):
    version_value = pins['version']
    signed = (directory / f'vibeface-{version_value}-ios.ipa').is_file()
    mobile = json.loads((release_context.ROOT / 'apps/mobile/app.json').read_text(encoding='utf-8'))['expo']
    identity_fields = dict(schema=1, platform='ios-device', source_sha=pins['source_commit'],
                           release_version=version_value, bundle_identifier=mobile['ios']['bundleIdentifier'],
                           bundle_short_version=mobile['version'], build_number=mobile['ios']['buildNumber'])
    state = 'signed' if signed else 'unsigned'
    payloads = [f'vibeface-{version_value}-ios-device-{state}.xcarchive.zip']
    if signed:
        payloads.append(f'vibeface-{version_value}-ios.ipa')
    for index, name in enumerate(payloads):
        payload = directory / name
        release_format.require(payload.is_file() and not payload.is_symlink(), 'missing regular iOS payload')
        verify_checksum_file(directory, name)
        receipt = json.loads((directory / (name + '.provenance.json')).read_text(encoding='utf-8'))
        fields = set(identity_fields) | {'artifact', 'sha256', 'signed', 'installable', 'kind'}
        if index:
            fields |= {'distribution_method', 'team_id', 'provisioning_profile_uuid', 'provisioning_profile_expiration', 'testflight_eligible'}
        release_format.require(set(receipt) == fields, 'unsupported iOS receipt fields')
        release_format.require(all(receipt.get(key) == value for key, value in identity_fields.items()), 'iOS source, version or app identity mismatch')
        release_format.require(receipt.get('artifact') == name and receipt.get('sha256') == digest(payload), 'iOS receipt payload digest mismatch')
        release_format.require(receipt.get('signed') is signed, 'iOS signing claim mismatch')
        installable = bool(index) and receipt.get('distribution_method') == 'ad-hoc'
        release_format.require(receipt.get('installable') is installable, 'iOS installability claim mismatch')
        if index:
            release_format.require(receipt.get('testflight_eligible') is (receipt.get('distribution_method') == 'app-store-connect'),
                    'iOS TestFlight eligibility claim mismatch')
        expected_kind = 'signed-ios-ipa' if index else f'{state}-xcarchive-inspection-only'
        release_format.require(receipt.get('kind') == expected_kind, 'iOS receipt must distinguish an archive from an installable IPA')
        if index:
            release_format.require(receipt.get('distribution_method') in ('app-store-connect', 'ad-hoc'), 'unsupported iOS distribution method')
            release_format.require(isinstance(receipt.get('team_id'), str) and re.fullmatch(r'[A-Z0-9]{10}', receipt['team_id']), 'invalid iOS signing team')
            release_format.require(isinstance(receipt.get('provisioning_profile_uuid'), str) and receipt['provisioning_profile_uuid'], 'missing iOS provisioning profile')
        with zipfile.ZipFile(payload) as archive:
            entries = archive.infolist()
            names = [entry.filename for entry in entries]
            release_format.require(len(names) == len(set(names)) and len(names) <= 32768, 'iOS archive has duplicate or excessive entries')
            release_format.require(all(not PurePosixPath(name).is_absolute() and '..' not in PurePosixPath(name).parts and '\\' not in name for name in names), 'unsafe iOS archive member path')
            info_name = 'Payload/Vibeface.app/Info.plist' if index else 'Vibeface.xcarchive/Products/Applications/Vibeface.app/Info.plist'
            profile_name = info_name.rsplit('/', 1)[0] + '/embedded.mobileprovision'
            release_format.require((profile_name in names) is signed, 'iOS archive provisioning state differs from its signing claim')
            info = plistlib.loads(archive.read(info_name))
            release_format.require((info.get('CFBundleIdentifier'), info.get('CFBundleShortVersionString'), str(info.get('CFBundleVersion')))
                    == (mobile['ios']['bundleIdentifier'], mobile['version'], str(mobile['ios']['buildNumber'])),
                    'iOS archive app metadata mismatch')
            release_format.require(info.get('CFBundleSupportedPlatforms') == ['iPhoneOS'], 'iOS device archive must not contain a simulator app')
    return signed


def verify_component(directory, app, expected=None):
    directory = Path(directory)
    info = release_format.validate_component(json.loads((directory / 'COMPONENT-INFO.json').read_text(encoding='utf-8')))
    release_format.require(info['app'] == app, 'unsupported component identity')
    if app == 'core':
        release_format.require(type(info['data_epoch']) is int and info['data_epoch'] == release_context.control_constant('DATA_EPOCH')
                and info['target'] in release_format.TARGETS, 'unsupported Core data epoch or target')
        release_format.require(set(info['files']) == {release_format.program_names(info['target'])[1]}, 'Core component must contain only its binary')
    else:
        prefix = 'web/' if app == 'app' else 'site/'
        release_format.require(app in ('app', 'www') and info['target'] is None
                and all(name.startswith(prefix) for name in info['files']), 'invalid static component layout')
    if expected:
        release_format.require(all(info[k] == value for k, value in expected.items()), 'component pin mismatch')
    release_format.require(info['files'] and info['files'] == files(directory, ('COMPONENT-INFO.json',)), 'component content changed')
    return info


def seal_bundle(directory, core_info=None, app_info=None):
    directory = Path(directory)
    release_format.require(not release_context.run('git', 'status', '--porcelain'), 'package source must be clean')
    source = release_context.run('git', 'rev-parse', 'HEAD')
    core_info = core_info or {'version': release_context.read_version('core'), 'source_commit': source}
    app_info = app_info or {'version': release_context.read_version('app'), 'source_commit': source}
    build = dict(version=release_context.read_version('cli'), target=os.environ['PACKAGE_TARGET'], source_commit=source,
                 **release_context.contracts(), core_version=core_info['version'], core_source_commit=core_info['source_commit'],
                 app_version=app_info['version'], app_source_commit=app_info['source_commit'])
    release_format.verify_build_info(build, release_context.contracts())
    write_json(directory / 'BUILD-INFO.json', build)
    (directory / 'FILES.sha256').write_text(''.join(f'{value}  {name}\n' for name, value in files(directory, ('FILES.sha256',)).items()),
                                           encoding='utf-8', newline='\n')
    return build


def native_receipt(native_dir, target, cli_version, source_commit):
    info = json.loads((Path(native_dir) / 'NATIVE-INFO.json').read_text())
    release_format.require(set(info) == {'schema', 'target', 'version', 'source_commit', 'files', 'data_epoch'}, 'unsupported native receipt')
    release_format.require(info['schema'] == release_format.SCHEMA_VERSION and info['target'] == target and info['version'] == cli_version
            and info['source_commit'] == source_commit and info['data_epoch'] == release_context.control_constant('DATA_EPOCH'), 'native staging identity mismatch')
    release_format.require(set(info['files']) == {release_format.program_names(target)[0]}
            and files(Path(native_dir), ('NATIVE-INFO.json',)) == info['files'], 'CLI native staging bytes differ')
    return info


def verify_checksum_file(directory, name):
    path = Path(directory) / name
    release_format.require(path.is_file() and not path.is_symlink(), 'missing release payload')
    checksum = path.with_name(path.name + '.sha256')
    release_format.require(checksum.is_file() and checksum.read_text(encoding='utf-8') == f'{digest(path)}  {name}\n', 'release checksum mismatch')


def verify_bundle(directory, target, info):
    build = release_format.verify_build_info(json.loads((directory / 'BUILD-INFO.json').read_text()), release_context.contracts())
    release_format.require(build['target'] == target and build['version'] == info['version'] and build['source_commit'] == info['source_commit'], 'CLI bundle identity mismatch')
    for dependency, pin in info['dependencies'].items():
        release_format.require(build[dependency + '_version'] == pin['version'] and build[dependency + '_source_commit'] == pin['source_commit'], 'CLI bundle dependency differs from manifest')
    declared = (directory / 'FILES.sha256').read_text()
    expected = ''.join(f'{value}  {name}\n' for name, value in files(directory, ('FILES.sha256',)).items())
    release_format.require(declared == expected, 'CLI sealed file checksums differ')
    return build
