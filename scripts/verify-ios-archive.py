#!/usr/bin/env python3
"""Validate a device archive and write source-bound public provenance."""
import argparse
import hashlib
import json
import plistlib
import subprocess
import zipfile
from pathlib import Path
from ios_device import verify_device_executable


def sha256_file(path):
    digest = hashlib.sha256()
    with path.open('rb') as source:
        for block in iter(lambda: source.read(1024 * 1024), b''):
            digest.update(block)
    return digest.hexdigest()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--archive', type=Path, required=True, help='Xcode .xcarchive directory')
    parser.add_argument('--zip', type=Path, required=True, help='Public archive ZIP')
    parser.add_argument('--source-sha', required=True)
    parser.add_argument('--release-version', required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--signed', action='store_true')
    args = parser.parse_args()

    root = Path(__file__).resolve().parent.parent
    if subprocess.check_output(['git', '-C', str(root), 'rev-parse', 'HEAD'], text=True).strip() != args.source_sha:
        raise SystemExit('checkout does not match source SHA')
    mobile = json.loads((root / 'apps/mobile/app.json').read_text())['expo']
    if mobile['extra']['releaseVersion'] != args.release_version:
        raise SystemExit('release version does not match committed Expo metadata')
    info_path = args.archive / 'Products/Applications/Vibeface.app/Info.plist'
    with info_path.open('rb') as source:
        info = plistlib.load(source)
    expected = {
        'CFBundleIdentifier': mobile['ios']['bundleIdentifier'],
        'CFBundleShortVersionString': mobile['version'],
        'CFBundleVersion': mobile['ios']['buildNumber'],
    }
    for key, value in expected.items():
        if info.get(key) != value:
            raise SystemExit(f'archive {key} differs from committed app metadata')
    if info.get('CFBundleSupportedPlatforms') != ['iPhoneOS']:
        raise SystemExit('archive app is not marked for iPhoneOS')
    app = info_path.parent
    executable = verify_device_executable(app, info)
    has_profile = (app / 'embedded.mobileprovision').exists()
    if has_profile != args.signed:
        raise SystemExit('archive signing state does not match the requested verification mode')
    if args.signed:
        subprocess.run(['codesign', '--verify', '--deep', '--strict', str(app)], check=True)
    if not args.zip.is_file() or not zipfile.is_zipfile(args.zip):
        raise SystemExit('missing or invalid archive ZIP')
    with zipfile.ZipFile(args.zip) as zipped:
        info_name = [name for name in zipped.namelist() if name.endswith('/Products/Applications/Vibeface.app/Info.plist')]
        executable_name = [name for name in zipped.namelist() if name.endswith('/Products/Applications/Vibeface.app/' + info['CFBundleExecutable'])]
        if len(info_name) != 1 or len(executable_name) != 1:
            raise SystemExit('ZIP does not contain exactly one expected device app bundle')
        for name, path in ((info_name[0], info_path), (executable_name[0], executable)):
            zipped_digest = hashlib.sha256()
            with zipped.open(name) as source:
                for block in iter(lambda: source.read(1024 * 1024), b''):
                    zipped_digest.update(block)
            if zipped_digest.hexdigest() != sha256_file(path):
                raise SystemExit('ZIP payload differs from the inspected Xcode archive')
    digest = sha256_file(args.zip)
    receipt = {
        'schema': 1,
        'platform': 'ios-device',
        'source_sha': args.source_sha,
        'release_version': args.release_version,
        'bundle_identifier': expected['CFBundleIdentifier'],
        'bundle_short_version': expected['CFBundleShortVersionString'],
        'build_number': expected['CFBundleVersion'],
        'artifact': args.zip.name,
        'sha256': digest,
        'signed': args.signed,
        'installable': False,
        'kind': 'signed-xcarchive-inspection-only' if args.signed else 'unsigned-xcarchive-inspection-only',
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(receipt, indent=2, sort_keys=True) + '\n')
    args.zip.with_name(args.zip.name + '.sha256').write_text(f'{digest}  {args.zip.name}\n')
    print(json.dumps(receipt, sort_keys=True))


if __name__ == '__main__':
    main()
