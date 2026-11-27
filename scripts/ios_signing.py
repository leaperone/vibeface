"""Validate the app and sharing extension's distribution signing together."""
import copy
import hashlib
import json
import plistlib
import re
import subprocess
import tempfile
from datetime import datetime, timedelta, timezone
from pathlib import Path
from ios_device import APP_GROUP, SHARE_BUNDLE_ID


APP_BUNDLE_ID = 'one.leaper.vibeface'


def validate_profile(profile, bundle_id, method):
    if not isinstance(profile, dict):
        raise ValueError('Provisioning profile must be a plist dictionary')
    if method not in ('app-store-connect', 'ad-hoc'):
        raise ValueError('Unsupported iOS distribution method')
    teams = profile.get('TeamIdentifier')
    uuid = profile.get('UUID')
    if (not isinstance(teams, list) or len(teams) != 1 or not isinstance(teams[0], str)
            or not re.fullmatch(r'[A-Z0-9]{10}', teams[0])
            or not isinstance(uuid, str) or not re.fullmatch(r'[0-9A-Fa-f]{8}(?:-[0-9A-Fa-f]{4}){3}-[0-9A-Fa-f]{12}', uuid)
            or not isinstance(profile.get('Name'), str) or not profile['Name'].strip()):
        raise ValueError('Provisioning profile is missing a valid identity')
    team = teams[0]
    entitlements = profile.get('Entitlements', {})
    if not isinstance(entitlements, dict) or entitlements.get('application-identifier') != f'{team}.{bundle_id}':
        raise ValueError('Provisioning profile does not exactly match its target bundle')
    if entitlements.get('com.apple.developer.team-identifier') != team:
        raise ValueError('Provisioning profile entitlement team does not match its identity')
    groups = entitlements.get('com.apple.security.application-groups')
    if not isinstance(groups, list) or APP_GROUP not in groups:
        raise ValueError('Provisioning profile does not allow the sharing App Group')
    if entitlements.get('get-task-allow') is not False or profile.get('ProvisionsAllDevices'):
        raise ValueError('Provisioning profile is not an App Store or Ad Hoc distribution profile')
    expiration = profile.get('ExpirationDate')
    if not isinstance(expiration, datetime) or expiration.replace(tzinfo=timezone.utc) <= datetime.now(timezone.utc):
        raise ValueError('Provisioning profile is expired or has no valid expiration')
    devices = profile.get('ProvisionedDevices', [])
    if not isinstance(devices, list) or any(not isinstance(device, str) or not device for device in devices):
        raise ValueError('Provisioning profile has invalid registered devices')
    if (method == 'ad-hoc') != bool(devices):
        raise ValueError('Provisioning profile does not match the requested distribution method')
    certificates = profile.get('DeveloperCertificates')
    if not isinstance(certificates, list) or not certificates or any(not isinstance(cert, bytes) or not cert for cert in certificates):
        raise ValueError('Provisioning profile has no valid distribution certificates')
    return dict(team=team, uuid=uuid, devices=devices,
                certificates=[hashlib.sha1(cert).hexdigest().upper() for cert in certificates])


def validate_pair(app_profile, share_profile, method):
    app = validate_profile(app_profile, APP_BUNDLE_ID, method)
    share = validate_profile(share_profile, SHARE_BUNDLE_ID, method)
    if app['uuid'].upper() == share['uuid'].upper() or app['team'] != share['team']:
        raise ValueError('App and sharing extension require distinct profiles from the same team')
    if not set(app['devices']).issubset(share['devices']):
        raise ValueError('The sharing extension profile must cover every registered app device')
    certificates = sorted(set(app['certificates']) & set(share['certificates']))
    if not certificates:
        raise ValueError('App and sharing extension profiles have no common signing certificate')
    return dict(team=app['team'], app_uuid=app['uuid'], share_uuid=share['uuid'], certificates=certificates)


def read_profile(path):
    return plistlib.loads(subprocess.check_output(['security', 'cms', '-D', '-i', str(path)]))


def verify_signature(bundle, profile, bundle_id):
    team = profile['TeamIdentifier'][0]
    subprocess.run(['codesign', '--verify', '--deep', '--strict', str(bundle)], check=True, capture_output=True)
    details = subprocess.run(['codesign', '-dv', '--verbose=4', str(bundle)], text=True, capture_output=True, check=True).stderr
    if not re.search(rf'^TeamIdentifier={team}$', details, re.M) or 'Authority=Apple Distribution:' not in details:
        raise ValueError('iOS bundle does not have an Apple Distribution signature for its profile team')
    entitlements = plistlib.loads(subprocess.check_output(['codesign', '-d', '--entitlements', ':-', str(bundle)], stderr=subprocess.PIPE))
    groups = entitlements.get('com.apple.security.application-groups', [])
    if (entitlements.get('application-identifier') != f'{team}.{bundle_id}'
            or entitlements.get('com.apple.developer.team-identifier') != team
            or entitlements.get('get-task-allow') is not False
            or not isinstance(groups, list) or APP_GROUP not in groups
            or not set(groups).issubset(profile['Entitlements']['com.apple.security.application-groups'])):
        raise ValueError('iOS bundle signature does not match its profile or sharing App Group')
    with tempfile.TemporaryDirectory(prefix='.signature-', dir=bundle.parent) as directory:
        prefix = str(Path(directory) / 'certificate')
        subprocess.run(['codesign', '-d', '--extract-certificates', prefix, str(bundle)], check=True, capture_output=True)
        certificate = Path(prefix + '0').read_bytes()
    fingerprint = hashlib.sha1(certificate).hexdigest().upper()
    allowed = {hashlib.sha1(cert).hexdigest().upper() for cert in profile['DeveloperCertificates']}
    if fingerprint not in allowed:
        raise ValueError('iOS signing certificate is not allowed by its embedded provisioning profile')
    return fingerprint


def verify_signed_pair(app, share, method):
    app_profile = read_profile(app / 'embedded.mobileprovision')
    share_profile = read_profile(share / 'embedded.mobileprovision')
    pair = validate_pair(app_profile, share_profile, method)
    app_certificate = verify_signature(app, app_profile, APP_BUNDLE_ID)
    share_certificate = verify_signature(share, share_profile, SHARE_BUNDLE_ID)
    if app_certificate != share_certificate:
        raise ValueError('App and sharing extension must use the same signing certificate')
    return app_profile, share_profile, pair['team']


def self_test():
    def fixture(bundle_id, uuid):
        return dict(UUID=uuid, Name='synthetic distribution profile', TeamIdentifier=['ABCDE12345'],
                    ExpirationDate=datetime.now(timezone.utc).replace(tzinfo=None) + timedelta(days=1),
                    DeveloperCertificates=[b'synthetic-certificate'], Entitlements={
                        'application-identifier': 'ABCDE12345.' + bundle_id, 'get-task-allow': False,
                        'com.apple.developer.team-identifier': 'ABCDE12345',
                        'com.apple.security.application-groups': [APP_GROUP]})
    app = fixture(APP_BUNDLE_ID, '11111111-1111-1111-1111-111111111111')
    share = fixture(SHARE_BUNDLE_ID, '22222222-2222-2222-2222-222222222222')
    validate_pair(app, share, 'app-store-connect')
    validate_pair(app | {'ProvisionedDevices': ['device-a']}, share | {'ProvisionedDevices': ['device-a', 'device-b']}, 'ad-hoc')
    invalid = [(app, [], 'app-store-connect'), (share, app, 'app-store-connect'), (app, share | {'UUID': app['UUID']}, 'app-store-connect'),
               (app, share | {'DeveloperCertificates': [b'other-certificate']}, 'app-store-connect'),
               (app, share, 'ad-hoc'), (app | {'ProvisionedDevices': ['device-a']}, share | {'ProvisionedDevices': ['device-b']}, 'ad-hoc')]
    for key, value in (('UUID', '../invalid'), ('Name', ''), ('TeamIdentifier', []), ('Entitlements', []),
                       ('ExpirationDate', 'tomorrow'), ('ExpirationDate', datetime.min), ('DeveloperCertificates', []),
                       ('DeveloperCertificates', ['not-DER']), ('ProvisionsAllDevices', True), ('ProvisionedDevices', ['device-a'])):
        invalid.append((app, share | {key: value}, 'app-store-connect'))
    for key, value in (('application-identifier', 'ABCDE12345.*'), ('get-task-allow', True),
                       ('com.apple.developer.team-identifier', 'OTHER12345'), ('com.apple.security.application-groups', [])):
        changed = copy.deepcopy(share)
        changed['Entitlements'][key] = value
        invalid.append((app, changed, 'app-store-connect'))
    changed = fixture(SHARE_BUNDLE_ID, share['UUID'])
    changed['TeamIdentifier'] = ['OTHER12345']
    changed['Entitlements']['application-identifier'] = 'OTHER12345.' + SHARE_BUNDLE_ID
    changed['Entitlements']['com.apple.developer.team-identifier'] = 'OTHER12345'
    invalid.append((app, changed, 'app-store-connect'))
    for values in invalid:
        try:
            validate_pair(*values)
        except ValueError:
            continue
        raise AssertionError('accepted invalid or crossed iOS distribution profiles')
    print('Synthetic dual-target iOS signing checks passed')


if __name__ == '__main__':
    import sys
    if sys.argv[1:] == ['--self-test']:
        self_test()
    elif len(sys.argv) == 4:
        profiles = [plistlib.loads(Path(path).read_bytes()) for path in sys.argv[1:3]]
        print(json.dumps(validate_pair(*profiles, sys.argv[3])))
    else:
        raise SystemExit('usage: ios_signing.py <app-profile.plist> <share-profile.plist> <method> | --self-test')
