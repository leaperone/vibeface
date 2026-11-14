#!/usr/bin/env python3
"""Select and stage one independently published Vibeface service."""
import argparse
import datetime
import json
from pathlib import Path
import re
import shutil
import subprocess
import tarfile
import tempfile
from unittest import TestCase
from unittest.mock import patch

import release

SERVICES = ('node', 'app', 'www')
DEPLOY_WORKFLOWS = {'node': 'deploy-node.yml', 'app': 'deploy-app.yml', 'www': 'deploy-www.yml'}
REPOSITORY = 'leaperone/vibeface'
CORE_TARGET = 'x86_64-unknown-linux-gnu'


def gh(*args):
    result = subprocess.run(['gh', *args], capture_output=True, text=True)
    message = ('Deployment dispatch result is unknown; inspect workflow runs for the selected tag before retrying'
               if args[:2] == ('workflow', 'run') else
               'GitHub request failed; inspect authenticated CLI access and retry the read-only plan')
    release.require(result.returncode == 0, message)
    return result.stdout


def service_for_app(app):
    release.require(app in ('core', 'app', 'www'), 'invalid release application')
    return 'node' if app == 'core' else app


def plan(tag):
    """Verify the selected app release and return the deploy contract."""
    kind, _, _ = release.parse_tag(tag)
    service = service_for_app(kind)
    info, _, manifest_sha256 = release.published(tag)
    release.require(info['app'] == kind and info['tag'] == tag,
                    'published release application differs from selected tag')
    return {'tag': tag, 'source_sha': info['source_commit'], 'version': info['version'],
            'channel': info['channel'], 'app': kind, 'service': service,
            'targets': [service], 'manifest_sha256': manifest_sha256}


def previous(tag):
    """Return the previous complete Core release for Node rehearsal, if one exists."""
    kind, _, _ = release.parse_tag(tag)
    release.require(kind == 'core', 'only Core releases have a Node rehearsal predecessor')
    selected = json.loads(gh('api', f'repos/{REPOSITORY}/releases/tags/{tag}'))
    release.require(selected.get('tag_name') == tag and not selected.get('draft'),
                    'candidate Core release is not published')
    published_at = selected.get('published_at')
    release.require(isinstance(published_at, str), 'candidate Core release has no publication time')
    cutoff = datetime.datetime.fromisoformat(published_at.replace('Z', '+00:00'))
    pages = json.loads(gh('api', '--paginate', '--slurp',
                          f'repos/{REPOSITORY}/releases?per_page=100'))
    release.require(isinstance(pages, list) and all(isinstance(page, list) for page in pages),
                    'invalid release listing')
    candidates = []
    for item in (entry for page in pages for entry in page):
        name = item.get('tag_name')
        if (item.get('draft') or name == tag or not isinstance(name, str)
                or not re.fullmatch('core-v' + release.VERSION, name)):
            continue
        date = item.get('published_at')
        if not isinstance(date, str):
            continue
        when = datetime.datetime.fromisoformat(date.replace('Z', '+00:00'))
        if when <= cutoff and any(asset.get('name') == 'release.json' for asset in item.get('assets', [])):
            candidates.append((when, name))
    if not candidates:
        return None
    return plan(max(candidates)[1])


def selection(service, tag):
    release.require(service in SERVICES, 'invalid deployment service')
    kind, _, _ = release.parse_tag(tag)
    release.require(service_for_app(kind) == service,
                    'release tag does not belong to the selected service')
    return [service]


def execute(action, service, tag):
    release.require(action in ('plan', 'release'), 'invalid deployment action')
    targets = selection(service, tag)
    info = plan(tag)
    release.require(info['service'] == service and info['targets'] == targets,
                    'deployment plan service mismatch')
    if action == 'release':
        gh('workflow', 'run', DEPLOY_WORKFLOWS[service], '--repo', REPOSITORY, '--ref', 'main',
           '-f', f'tag={tag}', '-f', 'apply=true')
    return info


def _selected_assets(info):
    suffix = '-x86_64-unknown-linux-gnu' if info['app'] == 'core' else ''
    name = f"vibeface-{info['app']}-{info['version']}{suffix}.tar.gz"
    return [name, name + '.sha256']


def prepare(service, tag, output):
    """Download and verify exactly the component owned by ``service``."""
    info = execute('plan', service, tag)
    output = Path(output)
    release.require(not output.exists(), 'deployment output already exists')
    output.mkdir(parents=True)
    downloads = output / 'downloads'
    downloads.mkdir()
    published_info, remote, manifest_sha256 = release.published(tag)
    release.require(published_info['app'] == info['app'], 'published app changed during preparation')
    names = _selected_assets(info)
    assets = {asset['name']: asset for asset in remote['assets']}
    selected = [assets[name] for name in names if name in assets]
    release.require(len(selected) == len(names), 'published release is missing the selected component')
    release.download_assets(tag, selected, downloads)
    archive_name = names[0]
    release.verify_checksum_file(downloads, archive_name)
    unpacked = release.unpack(downloads / archive_name, output / 'unpacked')
    expected = {'app': info['app'], 'tag': tag, 'version': info['version'],
                'source_commit': info['source_sha'], 'channel': info['channel'],
                'target': CORE_TARGET if info['app'] == 'core' else None}
    component = release.verify_component(unpacked, info['app'], expected)
    destination = output / info['app']
    unpacked.rename(destination)
    (output / 'unpacked').rmdir()
    info = dict(info, manifest_sha256=manifest_sha256, component=component)
    (output / 'deployment.json').write_text(json.dumps(info, indent=2) + '\n')
    return info


def self_test():
    source = 'a' * 40
    fake_info = {'schema': release.SCHEMA_VERSION, 'source_commit': source,
                 'channel': 'hourly', 'app': 'app',
                 'tag': 'app-v0.0.1-hourly.2026111300', 'version': '0.0.1-hourly.2026111300'}
    fake_remote = {'tag_name': fake_info['tag'], 'draft': False, 'prerelease': True, 'assets': []}
    with patch(__name__ + '.gh') as mocked_gh, \
            patch('release.published', return_value=(fake_info, fake_remote, 'c' * 64)):
        assert selection('app', fake_info['tag']) == ['app']
        assert plan(fake_info['tag'])['service'] == 'app'
        mocked_gh.assert_not_called()
    for service, tag in (('node', fake_info['tag']), ('app', 'www-v0.0.1')):
        with TestCase().assertRaises(ValueError, msg='accepted a tag for another service'):
            selection(service, tag)
    for service, tag in (('app', 'core-v0.0.1'), ('www', 'app-v0.0.1')):
        with TestCase().assertRaises(ValueError, msg='accepted a cross-service tag'):
            selection(service, tag)
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        source = 'b' * 40
        tag = 'app-v0.0.1-hourly.2026111300'
        component = root / 'vibeface-app-0.0.1-hourly.2026111300'
        (component / 'web').mkdir(parents=True)
        (component / 'web/_shell.html').write_text('fixture')
        component_info = {'app': 'app', 'schema': release.SCHEMA_VERSION, 'tag': tag,
                          'version': '0.0.1-hourly.2026111300', 'source_commit': source,
                          'channel': 'hourly', 'target': None,
                          'files': release.files(component)}
        (component / 'COMPONENT-INFO.json').write_text(json.dumps(component_info))
        archive = root / 'vibeface-app-0.0.1-hourly.2026111300.tar.gz'
        with tarfile.open(archive, 'w:gz') as output:
            output.add(component, arcname=component.name)
        checksum = archive.with_name(archive.name + '.sha256')
        checksum.write_text(f'{release.digest(archive)}  {archive.name}\n')
        info = {'tag': tag, 'source_sha': source, 'version': component_info['version'],
                'channel': 'hourly', 'app': 'app', 'service': 'app', 'targets': ['app']}
        remote = {'assets': [{'name': name, 'size': path.stat().st_size,
                              'digest': 'sha256:' + release.digest(path)}
                             for name, path in ((archive.name, archive), (checksum.name, checksum))]}

        def copy_assets(_tag, _selected, destination):
            shutil.copy2(archive, destination / archive.name)
            shutil.copy2(checksum, destination / checksum.name)

        with patch(__name__ + '.execute', return_value=info), \
                patch('release.published', return_value=(dict(info, app='app'), remote, 'd' * 64)), \
                patch('release.download_assets', side_effect=copy_assets):
            prepared = prepare('app', tag, root / 'prepared')
        assert prepared['manifest_sha256'] == 'd' * 64
        assert (root / 'prepared/app/web/_shell.html').read_text() == 'fixture'

        def copy_corrupt(_tag, _selected, destination):
            (destination / archive.name).write_bytes(b'corrupt')
            shutil.copy2(checksum, destination / checksum.name)

        with patch(__name__ + '.execute', return_value=info), \
                patch('release.published', return_value=(dict(info, app='app'), remote, 'd' * 64)), \
                patch('release.download_assets', side_effect=copy_corrupt), \
                TestCase().assertRaises(ValueError, msg='accepted a changed component archive'):
            prepare('app', tag, root / 'corrupt')
    print('deployment plan and service-boundary self-test passed')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='action', required=True)
    sub.add_parser('self-test')
    command = sub.add_parser('plan')
    command.add_argument('--tag', required=True)
    command = sub.add_parser('release')
    command.add_argument('--service', choices=SERVICES, required=True)
    command.add_argument('--tag', required=True)
    command = sub.add_parser('prepare')
    command.add_argument('--service', choices=SERVICES, required=True)
    command.add_argument('--tag', required=True)
    command.add_argument('--output', required=True)
    args = parser.parse_args()
    if args.action == 'self-test':
        self_test()
    elif args.action == 'plan':
        print(json.dumps(plan(args.tag)))
    elif args.action == 'release':
        print(json.dumps(execute('release', args.service, args.tag)))
    else:
        print(json.dumps(prepare(args.service, args.tag, args.output)))


if __name__ == '__main__':
    try:
        main()
    except (ValueError, KeyError, TypeError, json.JSONDecodeError, OSError) as error:
        raise SystemExit(str(error))
