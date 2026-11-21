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


def load_runtime_definitions(contract):
    """Return a defensive projection of parsed AUTO-16 runtime definitions.

    The loader is intentionally pure: callers own JSON/file parsing and pass
    the parsed contract explicitly.  This keeps release-format detached from
    checkout paths while giving Node/Python corpus adapters one shape to
    validate for field, target, and role drift.
    """
    require(isinstance(contract, dict) and not isinstance(contract, list), 'invalid runtime contract')
    require(contract.get('schema') == 1, 'unsupported runtime contract schema')
    build_info = contract.get('build_info')
    require(isinstance(build_info, dict), 'invalid runtime BUILD-INFO fields')
    fields = build_info.get('fields')
    require(isinstance(fields, list) and fields and all(isinstance(field, str) and field for field in fields),
            'invalid runtime BUILD-INFO fields')
    require(len(set(fields)) == len(fields), 'duplicate runtime BUILD-INFO field')
    types = build_info.get('types')
    require(isinstance(types, dict) and set(types) == set(fields), 'invalid runtime BUILD-INFO types')
    allowed_types = {'string', 'target', 'sha1', 'uint32'}
    require(all(isinstance(kind, str) and kind in allowed_types for kind in types.values()),
            'unsupported runtime BUILD-INFO type')
    web = contract.get('files', {}).get('web_required') if isinstance(contract.get('files'), dict) else None
    require(isinstance(web, list) and all(isinstance(name, str) and name for name in web), 'invalid runtime Web files')
    require(len(set(web)) == len(web), 'duplicate runtime Web file')
    rows = contract.get('targets')
    require(isinstance(rows, list) and rows, 'invalid runtime targets')
    expected = {'target', 'host', 'platform', 'flag', 'exe_suffix', 'service_suffix', 'installer', 'signing', 'arch', 'programs', 'roles'}
    targets = []
    for row in rows:
        require(isinstance(row, dict) and set(row) == expected, 'unsupported runtime target fields')
        for field in ('target', 'host', 'platform', 'flag', 'service_suffix', 'installer', 'signing'):
            require(isinstance(row[field], str) and row[field], f'invalid runtime target {field}')
        require(isinstance(row['exe_suffix'], str), 'invalid runtime target exe_suffix')
        require(row['arch'] is None or isinstance(row['arch'], str), 'invalid runtime target arch')
        programs = row['programs']
        require(isinstance(programs, list) and all(isinstance(name, str) and name for name in programs), 'invalid runtime programs')
        require(len(set(programs)) == len(programs), 'duplicate runtime program')
        roles = row['roles']
        require(isinstance(roles, dict), 'invalid runtime roles')
        require(set(roles) == {'runtime-core', 'node-full', 'npm-vendor', 'desktop-minimum'}, 'unsupported runtime roles')
        normalized_roles = {}
        for name, paths in roles.items():
            require(isinstance(name, str) and name and isinstance(paths, list)
                    and all(isinstance(path, str) and path for path in paths), 'invalid runtime role paths')
            require(len(set(paths)) == len(paths), f'duplicate runtime role path: {name}')
            normalized_roles[name] = tuple(paths)
        targets.append({**row, 'programs': tuple(programs), 'roles': normalized_roles})
    require(len({row['target'] for row in targets}) == len(targets), 'duplicate runtime target')
    return {'schema': contract['schema'], 'fields': tuple(fields), 'build_info_types': dict(types),
            'web_required': tuple(web), 'targets': tuple(targets)}


loadRuntimeDefinitions = load_runtime_definitions


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


def verify_build_info(info, expected_contracts, runtime):
    require(isinstance(runtime, dict) and isinstance(runtime.get('fields'), tuple)
            and isinstance(runtime.get('build_info_types'), dict), 'runtime definitions are required')
    fields = runtime['fields']
    types = runtime['build_info_types']
    require(isinstance(info, dict) and set(info) == set(fields), 'unsupported build info fields')
    targets = {row['target'] for row in runtime.get('targets', ())}
    for field in fields:
        kind = types.get(field)
        value = info[field]
        if kind == 'string':
            version(value)
        elif kind == 'target':
            require(isinstance(value, str) and value in targets, 'unsupported build target')
        elif kind == 'sha1':
            require(isinstance(value, str) and re.fullmatch(COMMIT, value), 'invalid build source')
        elif kind == 'uint32':
            if field in expected_contracts:
                require(type(value) is int and 0 <= value <= 0xffffffff and value == expected_contracts[field],
                        'unsupported build protocol or data epoch')
            else:
                require(type(value) is int and 0 <= value <= 0xffffffff, 'invalid build integer')
        else:
            require(False, 'unsupported build info type')
    for key, expected in expected_contracts.items():
        require(types.get(key) == 'uint32' and info[key] == expected, 'unsupported build protocol or data epoch')
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
