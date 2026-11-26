#!/usr/bin/env python3
"""Validate a device archive and write source-bound public provenance."""
import argparse
import hashlib
import json
import plistlib
import subprocess
import zipfile
from pathlib import Path
from ios_device import SHARE_TARGET, share_receipt, verify_bundle_info, verify_device_executable, verify_share_extension
from ios_signing import verify_signed_pair


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
    parser.add_argument('--distribution-method', choices=('app-store-connect', 'ad-hoc'), default='app-store-connect')
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
    verify_bundle_info(info, mobile['ios']['bundleIdentifier'], mobile['version'], mobile['ios']['buildNumber'])
    app = info_path.parent
    executable = verify_device_executable(app, info)
    extension, extension_info, extension_executable = verify_share_extension(app, mobile['version'], mobile['ios']['buildNumber'])
    if any((bundle / 'embedded.mobileprovision').exists() != args.signed for bundle in (app, extension)):
        raise SystemExit('archive signing state does not match the requested verification mode')
    if args.signed:
        verify_signed_pair(app, extension, args.distribution_method)
    if not args.zip.is_file() or not zipfile.is_zipfile(args.zip):
        raise SystemExit('missing or invalid archive ZIP')
    with zipfile.ZipFile(args.zip) as zipped:
        info_name = [name for name in zipped.namelist() if name.endswith('/Products/Applications/Vibeface.app/Info.plist')]
        executable_name = [name for name in zipped.namelist() if name.endswith('/Products/Applications/Vibeface.app/' + info['CFBundleExecutable'])]
        if len(info_name) != 1 or len(executable_name) != 1:
            raise SystemExit('ZIP does not contain exactly one expected device app bundle')
        extension_prefix = info_name[0].rsplit('/', 1)[0] + '/PlugIns/'
        extensions = {name[len(extension_prefix):].split('/', 1)[0] for name in zipped.namelist()
                      if name.startswith(extension_prefix) and '.appex/' in name[len(extension_prefix):]}
        if extensions != {SHARE_TARGET + '.appex'}:
            raise SystemExit('ZIP does not contain exactly the expected sharing extension')
        inspected = [(info_name[0], info_path), (executable_name[0], executable),
                     (extension_prefix + extension.name + '/Info.plist', extension / 'Info.plist'),
                     (extension_prefix + extension.name + '/' + extension_info['CFBundleExecutable'], extension_executable)]
        if args.signed:
            inspected.extend((info_name[0].rsplit('/', 1)[0] + '/' + str(path.relative_to(app)), path)
                             for path in (app / 'embedded.mobileprovision', extension / 'embedded.mobileprovision'))
        elif any(name.endswith('/embedded.mobileprovision') for name in zipped.namelist()):
            raise SystemExit('unsigned ZIP contains a provisioning profile')
        for name, path in inspected:
            if zipped.namelist().count(name) != 1:
                raise SystemExit('ZIP is missing a unique inspected bundle file')
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
        'share_extension': share_receipt(extension_info),
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
