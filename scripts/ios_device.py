"""Verify the program inside an iPhoneOS App, including each Mach-O slice."""
import re
import subprocess
import plistlib
from pathlib import Path


SHARE_BUNDLE_ID = 'one.leaper.vibeface.share'
SHARE_TARGET = 'expo-sharing-extension'
APP_GROUP = 'group.one.leaper.vibeface'


def verify_bundle_info(info, bundle_id, version, build_number, share=False):
    expected = (bundle_id, version, str(build_number))
    if (info.get('CFBundleIdentifier'), info.get('CFBundleShortVersionString'), str(info.get('CFBundleVersion'))) != expected:
        raise ValueError('iOS bundle identity or version differs from committed app metadata')
    if info.get('CFBundleSupportedPlatforms') != ['iPhoneOS']:
        raise ValueError('iOS bundle is not marked for iPhoneOS')
    group_key = 'AppGroupId' if share else 'ExpoShareIntoAppGroupId'
    if info.get(group_key) != APP_GROUP:
        raise ValueError('iOS bundle does not use the expected sharing App Group')
    if share and (info.get('CFBundleDisplayName') != 'Vibeface'
                  or info.get('CFBundlePackageType') != 'XPC!'
                  or info.get('NSExtension', {}).get('NSExtensionPointIdentifier') != 'com.apple.share-services'):
        raise ValueError('iOS sharing extension has an invalid display name or extension point')


def share_receipt(info):
    return {
        'bundle_identifier': info['CFBundleIdentifier'],
        'bundle_short_version': info['CFBundleShortVersionString'],
        'build_number': str(info['CFBundleVersion']),
        'display_name': info['CFBundleDisplayName'],
        'app_group_identifier': info['AppGroupId'],
        'extension_point_identifier': info['NSExtension']['NSExtensionPointIdentifier'],
    }


def verify_share_extension(app, version, build_number):
    extensions = list((app / 'PlugIns').glob('*.appex'))
    if len(extensions) != 1 or extensions[0].name != SHARE_TARGET + '.appex' or extensions[0].is_symlink():
        raise ValueError('iOS app must embed exactly its expected sharing extension')
    extension = extensions[0]
    with (extension / 'Info.plist').open('rb') as source:
        info = plistlib.load(source)
    verify_bundle_info(info, SHARE_BUNDLE_ID, version, build_number, share=True)
    executable = verify_device_executable(extension, info)
    return extension, info, executable


def require_device_commands(architectures, load_commands):
    if 'arm64' not in architectures.split():
        raise ValueError('iOS device executable has no arm64 architecture')
    commands = re.findall(r'Load command \d+\s+cmd LC_BUILD_VERSION(.*?)(?=Load command|\Z)', load_commands, re.S)
    platforms = [re.search(r'^\s*platform\s+(\d+)\s*$', command, re.M) for command in commands]
    if not platforms or any(platform is None or platform[1] != '2' for platform in platforms):
        raise ValueError('every Mach-O slice must target iPhoneOS, with no simulator platform')


def verify_device_executable(app, info):
    name = info.get('CFBundleExecutable')
    if not isinstance(name, str) or not re.fullmatch(r'[A-Za-z0-9_.-]+', name) or name in ('.', '..'):
        raise ValueError('iOS App has no valid executable name')
    executable = Path(app) / name
    if not executable.is_file() or executable.is_symlink():
        raise ValueError('iOS App executable must be a regular bundled file')
    require_device_commands(
        subprocess.check_output(['lipo', '-archs', str(executable)], text=True),
        subprocess.check_output(['otool', '-l', str(executable)], text=True),
    )
    return executable


def self_test():
    device = 'Load command 8\n      cmd LC_BUILD_VERSION\n  cmdsize 32\n platform 2\n    minos 18.0\n'
    simulator = device.replace('platform 2', 'platform 7')
    require_device_commands('arm64\n', device)
    for architectures, commands in [('x86_64', device), ('arm64', simulator), ('arm64', device + simulator), ('arm64', '')]:
        try:
            require_device_commands(architectures, commands)
        except ValueError:
            continue
        raise AssertionError('accepted an executable that does not target iPhoneOS')
    info = dict(CFBundleIdentifier=SHARE_BUNDLE_ID, CFBundleShortVersionString='0.0.1', CFBundleVersion='17',
                CFBundleSupportedPlatforms=['iPhoneOS'], AppGroupId=APP_GROUP, CFBundleDisplayName='Vibeface',
                CFBundlePackageType='XPC!', NSExtension={'NSExtensionPointIdentifier': 'com.apple.share-services'})
    verify_bundle_info(info, SHARE_BUNDLE_ID, '0.0.1', '17', share=True)
    for change in ({'CFBundleIdentifier': 'wrong.share'}, {'CFBundleVersion': '18'},
                   {'CFBundleSupportedPlatforms': ['iPhoneSimulator']}, {'AppGroupId': 'group.wrong'},
                   {'CFBundleDisplayName': SHARE_TARGET}, {'NSExtension': {}}, {'CFBundlePackageType': 'APPL'}):
        try:
            verify_bundle_info(info | change, SHARE_BUNDLE_ID, '0.0.1', '17', share=True)
        except ValueError:
            continue
        raise AssertionError('accepted invalid iOS sharing extension metadata')
    print('iOS executable platform checks passed')


if __name__ == '__main__':
    import sys
    if sys.argv[1:] != ['--self-test']:
        raise SystemExit('usage: python3 scripts/ios_device.py --self-test')
    self_test()
