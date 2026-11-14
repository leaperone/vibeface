#!/usr/bin/env python3
"""Select published product artifacts independently of publishing a release."""
import argparse
import datetime
import hashlib
import json
import re
from pathlib import Path
import subprocess
import sys
import tempfile
import tarfile
import shutil
from unittest.mock import patch

import release
import core_release
from unittest import TestCase

SERVICES = ('all', 'node', 'app', 'www')
DEPLOY_WORKFLOWS = {'node': 'deploy-node.yml', 'app': 'deploy-app.yml',
                    'www': 'deploy-www.yml', 'all': 'deploy-product.yml'}
REPOSITORY = 'leaperone/vibeface'


def gh(*args):
    result = subprocess.run(['gh', *args], capture_output=True, text=True)
    failure = ('Deployment dispatch result is unknown; inspect workflow runs for the selected tag before retrying'
               if args[:2] == ('workflow', 'run') else
               'GitHub request failed; inspect authenticated CLI access and retry the read-only plan')
    release.require(result.returncode == 0, failure)
    return result.stdout


def verify(tag, manifest, published, source_sha):
    kind, version, channel = release.parse_tag(tag)
    release.require(kind in ('product', 'www'), 'invalid deployment tag')
    release.verify_release_identity(manifest)
    release.require(manifest['app'] == kind and manifest['tag'] == tag and manifest['source_commit'] == source_sha,
                    'release source does not match the selected tag')
    release.require(published['tag_name'] == tag and not published['draft'], 'release is not published')
    release.require(published['prerelease'] == (channel != 'stable'), 'release channel mismatch')
    for asset in manifest['assets']:
        release.require(set(asset) == {'name', 'size', 'sha256'}, 'invalid asset fields')
        release.safe_name(asset['name'])
        release.require('/' not in asset['name'] and '\\' not in asset['name']
                        and asset['name'] not in ('.', '..', 'release.json'), 'invalid asset name')
    assets = {a['name']: a for a in manifest['assets']}
    actual = {a['name']: a for a in published['assets']}
    release.require(len(assets) == len(manifest['assets']) and len(actual) == len(published['assets']),
                    'duplicate release asset')
    required = ({f'vibeface-www-{version}.tar.gz'} if kind == 'www' else
                {f'vibeface-core-{version}-x86_64-unknown-linux-gnu.tar.gz',
                 f'vibeface-app-{version}.tar.gz'})
    required |= {name + '.sha256' for name in required}
    release.require(required <= assets.keys() and set(actual) == set(assets) | {'release.json'},
                    'product release is incomplete or contains unexpected assets')
    for name, asset in assets.items():
        release.require(type(asset['size']) is int and asset['size'] > 0
                        and re.fullmatch(r'[0-9a-f]{64}', asset['sha256'])
                        and actual[name]['size'] == asset['size']
                        and actual[name].get('digest') == 'sha256:' + asset['sha256'],
                        'invalid release asset metadata or digest')
    return {'tag': tag, 'source_sha': source_sha, 'version': version, 'channel': channel}


def verify_core_manifest(tag, manifest, published, source_sha):
    value, channel = core_release.parse_tag(tag)
    release.require(manifest['app'] == 'core' and manifest['tag'] == tag
                    and manifest['source_commit'] == source_sha
                    and manifest['version'] == value and manifest['channel'] == channel,
                    'Core release source does not match the selected tag')
    release.require(manifest['data_epoch'] == release.control_constant('DATA_EPOCH')
                    and manifest['update_protocol'] == release.control_constant('UPDATE_PROTOCOL'),
                    'Core release protocol or data epoch mismatch')
    assets = {asset['name']: asset for asset in manifest['assets']}
    expected = {core_release.CORE_ASSET.format(version=value), core_release.CORE_CHECKSUM.format(version=value)}
    release.require(set(assets) == {core_release.CORE_ASSET.format(version=value)}, 'Core release asset set mismatch')
    actual = {asset['name']: asset for asset in published['assets']}
    release.require(set(actual) == expected | {core_release.MANIFEST}, 'Core release has unexpected assets')
    return {'tag': tag, 'source_sha': source_sha, 'version': value, 'channel': channel,
            'app': 'core', 'data_epoch': manifest['data_epoch'], 'update_protocol': manifest['update_protocol']}


def plan(tag):
    if tag.startswith('core-v'):
        value, channel = core_release.parse_tag(tag)
        published = json.loads(gh('api', f'repos/{REPOSITORY}/releases/tags/{tag}'))
        assets = {asset['name']: asset for asset in published['assets']}
        manifest_asset = assets.get(core_release.MANIFEST)
        release.require(manifest_asset and manifest_asset.get('id'), 'published Core manifest is required')
        content = gh('api', '-H', 'Accept: application/octet-stream',
                     f"repos/{REPOSITORY}/releases/assets/{manifest_asset['id']}")
        release.require('sha256:' + hashlib.sha256(content.encode()).hexdigest() == manifest_asset.get('digest'),
                        'Core manifest digest mismatch')
        manifest = json.loads(content)
        obj = json.loads(gh('api', f'repos/{REPOSITORY}/git/ref/tags/{tag}'))['object']
        for _ in range(16):
            if obj['type'] == 'commit': break
            release.require(obj['type'] == 'tag', 'Core tag must resolve to a commit')
            obj = json.loads(gh('api', f"repos/{REPOSITORY}/git/tags/{obj['sha']}"))['object']
        source = obj['sha']
        fetch = ['git', 'fetch', '--tags']
        if release.run('git', 'rev-parse', '--is-shallow-repository') == 'true':
            fetch.append('--unshallow')
        subprocess.run([*fetch, 'origin', 'main', 'refs/tags/' + tag], cwd=release.ROOT, check=True)
        release.require(release.run('git', 'rev-parse', tag + '^{commit}') == source, 'Core tag source mismatch')
        release.require(core_release.source_context(source) is not None, 'Core source provenance failed')
        release.require(release.require_ci(core_release.source_context(source)) is not None, 'Core CI provenance failed')
        return verify_core_manifest(tag, manifest, published, source) | {'service': 'node', 'targets': ['node']}
    kind, _, _ = release.parse_tag(tag)
    release.require(kind in ('product', 'www'), 'invalid deployment tag')
    published = json.loads(gh('api', f'repos/{REPOSITORY}/releases/tags/{tag}'))
    manifests = [a for a in published['assets'] if a['name'] == 'release.json']
    release.require(len(manifests) == 1, 'published complete release.json is required')
    asset_id = manifests[0]['id']
    release.require(type(asset_id) is int and asset_id > 0, 'invalid manifest asset ID')
    content = gh('api', '-H', 'Accept: application/octet-stream',
                 f'repos/{REPOSITORY}/releases/assets/{asset_id}')
    release.require(len(content.encode('utf-8')) == manifests[0]['size']
                    and 'sha256:' + hashlib.sha256(content.encode('utf-8')).hexdigest() == manifests[0].get('digest'),
                    'release manifest digest mismatch')
    manifest = json.loads(content)
    obj = json.loads(gh('api', f'repos/{REPOSITORY}/git/ref/tags/{tag}'))['object']
    for _ in range(16):
        release.require(re.fullmatch(release.COMMIT, obj['sha']), 'invalid tag object')
        if obj['type'] == 'commit':
            break
        release.require(obj['type'] == 'tag', 'tag must resolve to a commit')
        obj = json.loads(gh('api', f"repos/{REPOSITORY}/git/tags/{obj['sha']}"))['object']
    else:
        raise ValueError('tag nesting is too deep')
    source = obj['sha']
    fetch = ['git', 'fetch', '--tags']
    if release.run('git', 'rev-parse', '--is-shallow-repository') == 'true':
        fetch.append('--unshallow')
    subprocess.run([*fetch, 'origin', 'main', 'refs/tags/' + tag], cwd=release.ROOT, check=True)
    release.require(release.run('git', 'rev-parse', tag + '^{commit}') == source, 'fetched release tag source mismatch')
    release.validate_source(source, kind)
    return verify(tag, manifest, published, source)


def previous(tag):
    """Select the last complete product release published no later than the candidate."""
    if tag.startswith('core-v'):
        selected = json.loads(gh('api', f'repos/{REPOSITORY}/releases/tags/{tag}'))
        cutoff = datetime.datetime.fromisoformat(selected['published_at'].replace('Z', '+00:00'))
        pages = json.loads(gh('api', '--paginate', '--slurp', f'repos/{REPOSITORY}/releases?per_page=100'))
        candidates = []
        for published in [item for page in pages for item in page]:
            name = published['tag_name']
            if published['draft'] or name == tag or not re.fullmatch('core-v' + release.VERSION, name):
                continue
            date = datetime.datetime.fromisoformat(published['published_at'].replace('Z', '+00:00'))
            if date <= cutoff and core_release.MANIFEST in {a['name'] for a in published['assets']}:
                candidates.append((date, name))
        if not candidates:
            return None
        return plan(max(candidates)[1])
    kind, _, _ = release.parse_tag(tag)
    release.require(kind == 'product', 'Core rehearsal requires a product release')
    selected = json.loads(gh('api', f'repos/{REPOSITORY}/releases/tags/{tag}'))
    release.require(selected['tag_name'] == tag and not selected['draft'], 'candidate release is not published')
    cutoff = datetime.datetime.fromisoformat(selected['published_at'].replace('Z', '+00:00'))
    pages = json.loads(gh('api', '--paginate', '--slurp', f'repos/{REPOSITORY}/releases?per_page=100'))
    release.require(isinstance(pages, list) and all(isinstance(page, list) for page in pages),
                    'invalid release listing')
    published_releases = [published for page in pages for published in page]
    release.require(any(published['tag_name'] == tag for published in published_releases),
                    'candidate is missing from the release listing')
    candidates = []
    for published in published_releases:
        name = published['tag_name']
        if published['draft'] or name == tag or not re.fullmatch('v' + release.VERSION, name):
            continue
        _, version, _ = release.parse_tag(name)
        names = {asset['name'] for asset in published['assets']}
        required = {f'vibeface-core-{version}-x86_64-unknown-linux-gnu.tar.gz',
                    f'vibeface-app-{version}.tar.gz'}
        required |= {asset_name + '.sha256' for asset_name in required} | {'release.json'}
        if not required <= names:
            continue
        date = datetime.datetime.fromisoformat(published['published_at'].replace('Z', '+00:00'))
        if date <= cutoff:
            candidates.append((date, name))
    if not candidates:
        return None
    # A final manifest claims completeness; failed verification must not fall back to an older release.
    return plan(max(candidates)[1])


def selection(service, tag):
    release.require(service in SERVICES, 'invalid deployment service')
    if tag.startswith('core-v'):
        release.require(service == 'node', 'Core releases can only deploy the public Node')
        return ['node']
    kind, _, _ = release.parse_tag(tag)
    release.require((service == 'www') == (kind == 'www'), 'website deployments require a www tag; other services require a product tag')
    return ['node', 'app'] if service == 'all' else [service]


def execute(action, service, tag):
    release.require(action in ('plan', 'release'), 'invalid deployment action')
    targets = selection(service, tag)
    info = plan(tag)
    info.update(service=service, targets=targets)
    if action == 'release':
        gh('workflow', 'run', DEPLOY_WORKFLOWS[service], '--repo', REPOSITORY, '--ref', 'main',
           '-f', f'tag={tag}', '-f', 'apply=true')
    return info


def prepare(service, tag, output):
    info = execute('plan', service, tag)
    output = Path(output)
    release.require(not output.exists(), 'deployment output already exists')
    output.mkdir(parents=True)
    if tag.startswith('core-v'):
        download = output / 'downloads'; download.mkdir(exist_ok=True)
        for name in (core_release.MANIFEST,
                     core_release.CORE_ASSET.format(version=info['version']),
                     core_release.CORE_CHECKSUM.format(version=info['version'])):
            result = subprocess.run(['gh', 'release', 'download', tag, '--repo', REPOSITORY,
                                     '--pattern', name, '--dir', str(download)], capture_output=True, text=True)
            release.require(result.returncode == 0, 'published Core component download failed')
        manifest = core_release.verify_manifest(download)
        context = output / 'core'; context.mkdir()
        archive = download / core_release.CORE_ASSET.format(version=info['version'])
        unpacked = release.unpack(archive, context / 'unpacked')
        shutil.move(str(unpacked), str(context / 'component'))
        (context / 'unpacked').rmdir()
        (output / 'deployment.json').write_text(json.dumps(info, indent=2) + '\n')
        return info
    raw = gh('api', f'repos/{REPOSITORY}/releases/tags/{tag}')
    published = json.loads(raw)
    manifests = [a for a in published['assets'] if a['name'] == 'release.json']
    release.require(len(manifests) == 1, 'missing final manifest')
    content = gh('api', '-H', 'Accept: application/octet-stream',
                 f"repos/{REPOSITORY}/releases/assets/{manifests[0]['id']}")
    release.require('sha256:' + hashlib.sha256(content.encode()).hexdigest() == manifests[0]['digest'],
                    'manifest changed before download')
    manifest = json.loads(content)
    verify(tag, manifest, published, info['source_sha'])
    expected = {'tag': tag, 'version': info['version'], 'source_commit': info['source_sha'],
                'channel': info['channel']}
    components = {'www'} if service == 'www' else set()
    if 'node' in info['targets']:
        components.add('core')
    if 'app' in info['targets']:
        components.add('app')
    for component in sorted(components):
        suffix = '-x86_64-unknown-linux-gnu' if component == 'core' else ''
        name = f"vibeface-{component}-{info['version']}{suffix}.tar.gz"
        entry = next(a for a in manifest['assets'] if a['name'] == name)
        download = output / 'downloads'; download.mkdir(exist_ok=True)
        result = subprocess.run(['gh', 'release', 'download', tag, '--repo', REPOSITORY,
                                 '--pattern', name, '--pattern', name + '.sha256', '--dir', str(download)],
                                capture_output=True, text=True)
        release.require(result.returncode == 0, 'published component download failed')
        archive = download / name
        release.require(archive.stat().st_size == entry['size'] and release.digest(archive) == entry['sha256'],
                        'downloaded component checksum mismatch')
        checksum = (download / (name + '.sha256')).read_text().split()
        release.require(checksum == [entry['sha256'], name], 'component checksum file mismatch')
        checksum_entry = next(a for a in manifest['assets'] if a['name'] == name + '.sha256')
        release.require(release.digest(download / (name + '.sha256')) == checksum_entry['sha256'],
                        'checksum file differs from release')
        context = output / component; context.mkdir()
        unpacked = release.unpack(archive, context / 'unpacked')
        target = 'x86_64-unknown-linux-gnu' if component == 'core' else None
        release.verify_component(unpacked, component, dict(expected, target=target))
        unpacked.rename(context / 'component')
        (context / 'unpacked').rmdir()
    (output / 'deployment.json').write_text(json.dumps(info, indent=2) + '\n')
    return info


def self_test():
    import copy
    tag = 'v0.0.1-hourly.2026110812'
    sha = 'a' * 40
    names = release.expected_asset_names('product', tag[1:], False) - {'release.json'}
    manifest = {'schema': release.SCHEMA_VERSION, 'app': 'product', 'tag': tag,
                'version': tag[1:], 'channel': 'hourly', 'source_commit': sha,
                'assets': [{'name': n, 'size': 1, 'sha256': 'b' * 64} for n in sorted(names)]}
    published = {'tag_name': tag, 'draft': False, 'prerelease': True,
                 'assets': [{'name': n, 'size': 1, 'digest': 'sha256:' + 'b' * 64}
                            for n in sorted(names | {'release.json'})]}
    assert verify(tag, manifest, published, sha)['source_sha'] == sha
    core_tag = 'core-v0.0.1-hourly.2026111300'
    core_asset = core_release.CORE_ASSET.format(version=core_tag[len('core-v'):])
    core_checksum = core_release.CORE_CHECKSUM.format(version=core_tag[len('core-v'):])
    core_manifest = {
        'app': 'core', 'tag': core_tag, 'version': core_tag[len('core-v'):],
        'channel': 'hourly', 'source_commit': sha,
        'data_epoch': release.control_constant('DATA_EPOCH'),
        'update_protocol': release.control_constant('UPDATE_PROTOCOL'),
        'assets': [{'name': core_asset, 'size': 1, 'sha256': 'b' * 64}],
    }
    core_published = {'assets': [{'name': name} for name in (core_asset, core_checksum, core_release.MANIFEST)]}
    assert verify_core_manifest(core_tag, core_manifest, core_published, sha)['source_sha'] == sha
    with TestCase().assertRaises(ValueError, msg='Core release without its checksum was accepted'):
        verify_core_manifest(core_tag, core_manifest, {'assets': [{'name': core_asset}, {'name': core_release.MANIFEST}]}, sha)
    for value in ('0.0.1', '0.0.1-rc.1', '0.0.1-hourly.2026110712'):
        m = copy.deepcopy(manifest); p = copy.deepcopy(published)
        m.update(tag='v' + value, version=value, channel=release.version(value))
        p.update(tag_name=m['tag'], prerelease=m['channel'] != 'stable')
        for item in m['assets'] + p['assets']:
            item['name'] = item['name'].replace(tag[1:], value)
        assert verify(m['tag'], m, p, sha)['version'] == value
    historical = copy.deepcopy(manifest); historical_public = copy.deepcopy(published)
    required = {f'vibeface-core-{tag[1:]}-x86_64-unknown-linux-gnu.tar.gz', f'vibeface-app-{tag[1:]}.tar.gz'}
    required |= {name + '.sha256' for name in required}
    historical['assets'] = [a for a in historical['assets'] if a['name'] in required]
    historical_public['assets'] = [a for a in historical_public['assets'] if a['name'] in required | {'release.json'}]
    assert verify(tag, historical, historical_public, sha)
    for field, value in [('draft', True), ('prerelease', False), ('tag_name', 'v0.0.1')]:
        bad = dict(published, **{field: value})
        with TestCase().assertRaises(ValueError, msg=field):
            verify(tag, manifest, bad, sha)
    for mutate in (lambda m: m['assets'].pop(),
                   lambda m: m.update(source_commit='c' * 40),
                   lambda m: m['assets'].append(m['assets'][0]),
                   lambda m: m['assets'][0].update(sha256='invalid')):
        bad = copy.deepcopy(manifest); mutate(bad)
        with TestCase().assertRaises(ValueError, msg='accepted invalid manifest'):
            verify(tag, bad, published, sha)
    content = json.dumps(historical)
    remote = copy.deepcopy(historical_public)
    manifest_asset = next(a for a in remote['assets'] if a['name'] == 'release.json')
    manifest_asset.update(id=1, size=len(content.encode()),
                          digest='sha256:' + hashlib.sha256(content.encode()).hexdigest())
    routes = {
        f'repos/{REPOSITORY}/releases/tags/{tag}': json.dumps(remote),
        f'repos/{REPOSITORY}/releases/assets/1': content,
        f'repos/{REPOSITORY}/git/ref/tags/{tag}': json.dumps({'object': {'type': 'tag', 'sha': 'd' * 40}}),
        f'repos/{REPOSITORY}/git/tags/{"d" * 40}': json.dumps({'object': {'type': 'commit', 'sha': sha}}),
    }
    with patch(__name__ + '.gh', side_effect=lambda *args: routes[args[-1]]), \
            patch('release.run', return_value=sha), patch('subprocess.run'), patch('release.validate_source') as source_gate:
        assert plan(tag)['source_sha'] == sha
        source_gate.assert_called_once_with(sha, 'product')
        for route, bad in ((f'repos/{REPOSITORY}/releases/assets/1', content + ' '),):
            saved = routes[route]; routes[route] = bad
            with TestCase().assertRaises(ValueError, msg='accepted changed manifest or non-main source'):
                plan(tag)
            routes[route] = saved
    def listed(name, date, draft=False, complete=True):
        names = {f'vibeface-core-{name[1:]}-x86_64-unknown-linux-gnu.tar.gz',
                 f'vibeface-app-{name[1:]}.tar.gz'}
        names |= {name + '.sha256' for name in names}
        if complete:
            names.add('release.json')
        return {'tag_name': name, 'draft': draft, 'published_at': date,
                'assets': [{'name': value} for value in sorted(names)]}
    baseline = 'v0.0.1-hourly.2026110712'
    candidate = listed(tag, '2026-11-08T12:00:00Z')
    pages = [[listed('v0.0.1-hourly.2026111012', '2026-11-10T12:00:00Z'), candidate,
              listed('v0.0.1-hourly.2026110811', '2026-11-08T11:00:00Z', draft=True),
              listed('v0.0.1-hourly.2026110810', '2026-11-08T10:00:00Z', complete=False),
              listed('www-v0.0.1', '2026-11-08T11:00:00Z')],
             [listed('v0.0.1-hourly.2026110612', '2026-11-06T12:00:00Z'),
              listed(baseline, '2026-11-07T12:00:00Z')]]
    missing_component = listed('v0.0.1-hourly.2026110813', '2026-11-08T11:30:00Z')
    missing_component['assets'].pop(0)
    pages[0].append(missing_component)
    for response, expected in ((pages, baseline), ([pages[0]], None)):
        with patch(__name__ + '.gh', side_effect=[json.dumps(candidate), json.dumps(response)]) as query, \
                patch(__name__ + '.plan', return_value={'tag': baseline}) as verify_previous:
            assert previous(tag) == ({'tag': expected} if expected else None)
            query.assert_called_with('api', '--paginate', '--slurp', f'repos/{REPOSITORY}/releases?per_page=100')
            if expected:
                verify_previous.assert_called_once_with(expected)
            else:
                verify_previous.assert_not_called()
    with patch(__name__ + '.gh', side_effect=[json.dumps(candidate), json.dumps(pages)]), \
            patch(__name__ + '.plan', side_effect=ValueError('invalid previous manifest')) as verify_previous:
        try:
            previous(tag)
        except ValueError as error:
            assert str(error) == 'invalid previous manifest'
        else:
            raise AssertionError('fell back after a previous release verification failure')
        verify_previous.assert_called_once_with(baseline)
    for response in (dict(candidate, published_at='invalid'), dict(candidate, draft=True)):
        with patch(__name__ + '.gh', return_value=json.dumps(response)):
            try:
                previous(tag)
            except ValueError:
                pass
            else:
                raise AssertionError('invalid release metadata accepted as a first release')
    for response in ([], [[]], ['invalid page']):
        with patch(__name__ + '.gh', side_effect=[json.dumps(candidate), json.dumps(response)]):
            try:
                previous(tag)
            except ValueError:
                pass
            else:
                raise AssertionError('invalid release listing accepted as a first release')
    with patch(__name__ + '.gh', side_effect=ValueError('offline GitHub failure')):
        try:
            previous(tag)
        except ValueError as error:
            assert str(error) == 'offline GitHub failure'
        else:
            raise AssertionError('GitHub failure accepted as a first release')
    for service, workflow, selected_tag, targets in (
            ('node', 'deploy-node.yml', tag, ['node']),
            ('app', 'deploy-app.yml', tag, ['app']),
            ('www', 'deploy-www.yml', 'www-v0.0.1', ['www']),
            ('all', 'deploy-product.yml', tag, ['node', 'app'])):
        with patch(__name__ + '.plan', return_value={'tag': selected_tag, 'source_sha': sha}) as verify_plan, \
                patch(__name__ + '.gh') as command:
            assert execute('plan', service, selected_tag)['targets'] == targets
            verify_plan.assert_called_once_with(selected_tag)
            command.assert_not_called()
            verify_plan.reset_mock()
            assert execute('release', service, selected_tag)['targets'] == targets
            verify_plan.assert_called_once_with(selected_tag)
            command.assert_called_once_with('workflow', 'run', workflow, '--repo', REPOSITORY,
                                           '--ref', 'main', '-f', f'tag={selected_tag}', '-f', 'apply=true')
    with patch(__name__ + '.plan', side_effect=ValueError('unverified release')), \
            patch(__name__ + '.gh') as command:
        with TestCase().assertRaisesRegex(ValueError, 'unverified release'):
            execute('release', 'node', tag)
        command.assert_not_called()
    with patch(__name__ + '.plan', return_value={'tag': tag, 'source_sha': sha}), \
            patch('subprocess.run', return_value=subprocess.CompletedProcess(['gh'], 1, '', '')) as dispatch:
        with TestCase().assertRaisesRegex(ValueError, 'dispatch result is unknown'):
            execute('release', 'app', tag)
        assert dispatch.call_count == 1, 'retried a deployment with an unknown result'
    assert selection('all', tag) == ['node', 'app']
    assert selection('www', 'www-v0.0.1') == ['www']
    core_tag = 'core-v0.0.1-hourly.2026111300'
    assert selection('node', core_tag) == ['node']
    for service in ('all', 'app', 'www'):
        with TestCase().assertRaises(ValueError, msg='accepted Core tag for an incompatible service'):
            selection(service, core_tag)
    for service, wrong_tag in (('www', tag), ('node', 'www-v0.0.1'), ('app', 'www-v0.0.1'),
                               ('all', 'www-v0.0.1'), ('invalid', tag)):
        with patch(__name__ + '.plan') as verify_plan, patch(__name__ + '.gh') as command:
            with TestCase().assertRaises(ValueError, msg='accepted wrong deployment group'):
                execute('release', service, wrong_tag)
            verify_plan.assert_not_called()
            command.assert_not_called()
    scratch = release.ROOT / '.artifacts'
    scratch.mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(dir=scratch, prefix='deploy-check-') as temp:
        root = Path(temp)
        directory = root / 'vibeface-app'; (directory / 'web').mkdir(parents=True)
        (directory / 'web/_shell.html').write_text('released fixture')
        component = {'app': 'app', 'schema': 1, 'tag': tag, 'version': tag[1:],
                     'source_commit': sha, 'channel': 'hourly', 'target': None,
                     'files': release.files(directory)}
        def fixture(source=sha, corrupt=False, bad_checksum=False):
            component['source_commit'] = source
            (directory / 'COMPONENT-INFO.json').write_text(json.dumps(component))
            name = f'vibeface-app-{tag[1:]}.tar.gz'
            archive = root / name
            with tarfile.open(archive, 'w:gz') as output:
                output.add(directory, arcname=directory.name)
            if corrupt:
                archive.write_bytes(b'invalid gzip')
            checksum = root / (name + '.sha256')
            checksum.write_text(('0' * 64 if bad_checksum else release.digest(archive)) + '  ' + name + '\n')
            m = copy.deepcopy(historical)
            for a in m['assets']:
                if a['name'] in (name, name + '.sha256'):
                    path = root / a['name']; a.update(size=path.stat().st_size, sha256=release.digest(path))
            content = json.dumps(m)
            published = {'tag_name': tag, 'draft': False, 'prerelease': True, 'assets':
                         [dict(a, digest='sha256:' + a['sha256']) for a in m['assets']] +
                         [{'name': 'release.json', 'id': 1, 'size': len(content.encode()),
                           'digest': 'sha256:' + hashlib.sha256(content.encode()).hexdigest()}]}
            return json.dumps(published), content
        def download(args, **kwargs):
            destination = Path(args[args.index('--dir') + 1])
            for name in (f'vibeface-app-{tag[1:]}.tar.gz', f'vibeface-app-{tag[1:]}.tar.gz.sha256'):
                shutil.copy2(root / name, destination / name)
            return subprocess.CompletedProcess(args, 0, '', '')
        for index, values in enumerate(({}, {'corrupt': True}, {'bad_checksum': True}, {'source': 'f' * 40})):
            remote, payload = fixture(**values)
            with patch(__name__ + '.execute', return_value={'tag': tag, 'source_sha': sha, 'version': tag[1:],
                       'channel': 'hourly', 'targets': ['app']}), \
                    patch(__name__ + '.gh', side_effect=[remote, payload]), \
                    patch('subprocess.run', side_effect=download):
                try:
                    prepare('app', tag, root / f'prepared-{index}')
                except (ValueError, OSError, tarfile.TarError):
                    assert index != 0, 'valid component was refused'
                else:
                    assert index == 0, 'corrupt, bad-checksum or foreign component was accepted'
    print('deployment command self-test passed')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='action', required=True)
    sub.add_parser('self-test')
    for action in ('plan', 'release', 'prepare'):
        command = sub.add_parser(action)
        command.add_argument('--service', choices=SERVICES, required=True)
        command.add_argument('--tag', required=True, help='Exact existing published tag')
        if action == 'prepare':
            command.add_argument('--output', required=True)
    args = parser.parse_args()
    if args.action == 'self-test':
        self_test()
    elif args.action == 'prepare':
        print(json.dumps(prepare(args.service, args.tag, args.output)))
    else:
        print(json.dumps(execute(args.action, args.service, args.tag)))


if __name__ == '__main__':
    try:
        main()
    except (ValueError, KeyError, TypeError, json.JSONDecodeError) as error:
        sys.exit(str(error))
