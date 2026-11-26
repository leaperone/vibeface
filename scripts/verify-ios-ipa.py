#!/usr/bin/env python3
"""Verify an exported iOS IPA against its embedded distribution profile and app identity."""
import argparse
import hashlib
import json
import plistlib
import re
import stat
import subprocess
import tempfile
import zipfile
from pathlib import Path, PurePosixPath
from ios_device import share_receipt, verify_bundle_info, verify_device_executable, verify_share_extension
from ios_signing import verify_signed_pair


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--ipa', type=Path, required=True)
    parser.add_argument('--release-version', required=True)
    parser.add_argument('--source-sha', required=True)
    parser.add_argument('--distribution-method', choices=('app-store-connect', 'ad-hoc'), required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    if subprocess.check_output(['git', '-C', str(root), 'rev-parse', 'HEAD'], text=True).strip() != args.source_sha:
        raise SystemExit('checkout does not match source SHA')
    mobile = json.loads((root / 'apps/mobile/app.json').read_text())['expo']
    if mobile['extra']['releaseVersion'] != args.release_version:
        raise SystemExit('release version does not match committed Expo metadata')
    if not args.ipa.is_file() or not zipfile.is_zipfile(args.ipa):
        raise SystemExit('missing or invalid IPA')
    with tempfile.TemporaryDirectory(prefix='.vibeface-ios-ipa-', dir=args.ipa.parent) as directory:
        with zipfile.ZipFile(args.ipa) as ipa:
            members = ipa.infolist()
            if len(members) > 100_000 or sum(member.file_size for member in members) > 2_147_483_648:
                raise SystemExit('IPA exceeds safe extraction limits')
            names = []
            seen = set()
            for member in members:
                name = member.filename
                normalized_name = str(PurePosixPath(name.rstrip('/')))
                parts = name.rstrip('/').split('/')
                if (not name or name.startswith('/') or '\\' in name or
                        '\0' in name or any(part in ('', '.', '..') for part in parts) or
                        stat.S_ISLNK(member.external_attr >> 16) or member.flag_bits & 1):
                    raise SystemExit('IPA contains an unsafe or encrypted ZIP member')
                if normalized_name in seen:
                    raise SystemExit('IPA contains duplicate ZIP members')
                seen.add(normalized_name)
                names.append(name)
            candidates = [name for name in names if re.fullmatch(r'Payload/[^/]+\.app/Info\.plist', name)]
            if len(candidates) != 1:
                raise SystemExit('IPA must contain exactly one app bundle')
            ipa.extractall(directory)
        app = Path(directory) / candidates[0].rsplit('/', 1)[0]
        info = plistlib.loads((app / 'Info.plist').read_bytes())
        verify_bundle_info(info, mobile['ios']['bundleIdentifier'], mobile['version'], mobile['ios']['buildNumber'])
        verify_device_executable(app, info)
        extension, extension_info, _ = verify_share_extension(app, mobile['version'], mobile['ios']['buildNumber'])
        profile, share_profile, team = verify_signed_pair(app, extension, args.distribution_method)
    digest = hashlib.sha256()
    with args.ipa.open('rb') as source:
        for block in iter(lambda: source.read(1024 * 1024), b''):
            digest.update(block)
    value = digest.hexdigest()
    receipt = {
        'schema': 1, 'platform': 'ios-device', 'source_sha': args.source_sha,
        'release_version': args.release_version, 'bundle_identifier': mobile['ios']['bundleIdentifier'],
        'bundle_short_version': mobile['version'], 'build_number': mobile['ios']['buildNumber'],
        'share_extension': share_receipt(extension_info),
        'artifact': args.ipa.name, 'sha256': value, 'signed': True,
        'installable': args.distribution_method == 'ad-hoc',
        'testflight_eligible': args.distribution_method == 'app-store-connect',
        'distribution_method': args.distribution_method, 'team_id': team,
        'provisioning_profile_uuid': profile['UUID'], 'provisioning_profile_expiration': profile['ExpirationDate'].isoformat(),
        'share_provisioning_profile_uuid': share_profile['UUID'],
        'share_provisioning_profile_expiration': share_profile['ExpirationDate'].isoformat(),
        'kind': 'signed-ios-ipa',
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(receipt, indent=2, sort_keys=True) + '\n')
    args.ipa.with_name(args.ipa.name + '.sha256').write_text(f'{value}  {args.ipa.name}\n')
    print(json.dumps(receipt, sort_keys=True))


if __name__ == '__main__':
    main()
