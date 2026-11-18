"""Pure release formats, asset names and explicitly supplied identity contracts."""
from pathlib import PurePosixPath
import re
from release_component_identity import (APPS, COMPONENTS, VERSION, COMMIT, SHA256, SCHEMA_VERSION,
                                        require, version, parse_tag, validate_component)


TARGETS = ('aarch64-apple-darwin', 'aarch64-unknown-linux-gnu', 'x86_64-unknown-linux-gnu', 'x86_64-pc-windows-msvc')


WINDOWS_TARGETS = ('x86_64-pc-windows-msvc',)


DESKTOP_INSTALLERS = {'aarch64-apple-darwin': 'dmg', 'x86_64-pc-windows-msvc': 'exe',
                      'x86_64-unknown-linux-gnu': 'AppImage', 'aarch64-unknown-linux-gnu': 'AppImage'}


NATIVE_UI = {'ui': 'gpui', 'gpui_kit': '0.9.1', 'gpui': '0.3.12'}


DESKTOP_SCHEMA = 2


SIGNING_STATES = ('developer-id-notarized', 'authenticode', 'unsigned-development-preview', 'platform-checksums')


GITHUB_REPO = 'leaperone/vibeface'


DEPENDENCIES = {'core': (), 'app': (), 'cli': ('core', 'app'), 'desktop': ('cli',), 'mobile': (), 'www': ()}


WORKFLOWS = {app: f'release-{app}.yml' for app in APPS} | {'cli': 'release.yml', 'desktop': 'release-desktop-app.yml', 'mobile': 'release-mobile.yml'}


def program_suffix(target):
    return '.exe' if target in WINDOWS_TARGETS else ''


def program_names(target):
    return tuple(name + program_suffix(target) for name in ('vibeface', 'vibeface-core'))


def release_tag_for(app, value):
    require(app in APPS, 'unknown application')
    version(value)
    return f'{app}-v{value}'


def validate_pin(app, pin):
    require(isinstance(pin, dict) and set(pin) == {'tag', 'version', 'source_commit', 'manifest_sha256'}, 'invalid dependency pin fields')
    require(parse_tag(pin['tag'])[:2] == (app, pin['version']), 'dependency tag/version mismatch')
    require(isinstance(pin['source_commit'], str) and re.fullmatch(COMMIT, pin['source_commit'])
            and isinstance(pin['manifest_sha256'], str) and re.fullmatch(SHA256, pin['manifest_sha256']), 'invalid dependency identity')
    return pin


def safe_name(name):
    path = PurePosixPath(name)
    require(name and not path.is_absolute() and all(p not in ('', '.', '..') for p in name.split('/'))
            and re.fullmatch(r'[a-zA-Z0-9._/-]+', name), 'unsafe archive path')


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


def apk_asset_names(version_value):
    name = f'vibeface-{version_value}-android.apk'
    return {name, name + '.sha256', name + '.provenance.json'}


def ios_asset_names(version_value, signed=False):
    state = 'signed' if signed else 'unsigned'
    payloads = [f'vibeface-{version_value}-ios-device-{state}.xcarchive.zip']
    if signed:
        payloads.append(f'vibeface-{version_value}-ios.ipa')
    return {name + suffix for name in payloads for suffix in ('', '.sha256', '.provenance.json')}


def verify_build_info(info, expected_contracts):
    require(set(info) == {'version', 'target', 'source_commit', 'update_protocol', 'data_epoch',
                          'core_version', 'core_source_commit', 'app_version', 'app_source_commit'}, 'unsupported build info fields')
    require(info['target'] in TARGETS, 'unsupported build target')
    for key, expected in expected_contracts.items():
        require(type(info[key]) is int and info[key] == expected, 'unsupported build protocol or data epoch')
    for key in ('source_commit', 'core_source_commit', 'app_source_commit'):
        require(isinstance(info[key], str) and re.fullmatch(COMMIT, info[key]), 'invalid build source')
    for key in ('version', 'core_version', 'app_version'):
        version(info[key])
    return info


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


def verify_release_identity(info, expected_contracts):
    require(isinstance(info, dict) and set(info) == {'schema', 'app', 'version', 'tag', 'source_commit', 'channel', 'contracts', 'dependencies', 'assets'},
            'unsupported release manifest fields')
    require(type(info['schema']) is int and info['schema'] == SCHEMA_VERSION and info['app'] in APPS, 'invalid release manifest schema/application')
    require(parse_tag(info['tag']) == (info['app'], info['version'], info['channel']), 'invalid release identity')
    require(isinstance(info['source_commit'], str) and re.fullmatch(COMMIT, info['source_commit']), 'invalid release source')
    require(info['contracts'] == expected_contracts and all(type(v) is int for v in info['contracts'].values()), 'release contract mismatch')
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
