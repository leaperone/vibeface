#!/usr/bin/env python3
"""Select published product artifacts independently of publishing a release."""
import argparse
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

SERVICES = ('all', 'discovery', 'relay', 'relay-cn', 'app', 'www')
REPOSITORY = 'leaperone/vibeface'


def gh(*args):
    result = subprocess.run(['gh', *args], capture_output=True, text=True)
    release.require(result.returncode == 0, 'GitHub request failed; inspect authenticated CLI access and retry the read-only plan')
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


def plan(tag):
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
    comparison = json.loads(gh('api', f'repos/{REPOSITORY}/compare/{source}...main'))
    release.require(comparison['merge_base_commit']['sha'] == source, 'release is not on main')
    return verify(tag, manifest, published, source)


def selection(service, tag):
    kind, _, _ = release.parse_tag(tag)
    release.require((service == 'www') == (kind == 'www'), 'website deployments require a www tag; other services require a product tag')
    return ['discovery', 'relay', 'relay-cn', 'app'] if service == 'all' else [service]


def execute(action, service, tag):
    targets = selection(service, tag)
    info = plan(tag)
    info.update(service=service, targets=targets)
    if action == 'release':
        gh('workflow', 'run', 'deploy-services.yml', '--repo', REPOSITORY, '--ref', 'main',
           '-f', f'service={service}', '-f', f'tag={tag}', '-f', 'apply=true')
    return info


def prepare(service, tag, output):
    info = execute('plan', service, tag)
    output = Path(output)
    release.require(not output.exists(), 'deployment output already exists')
    output.mkdir(parents=True)
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
    if any(t in ('discovery', 'relay', 'relay-cn') for t in info['targets']):
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
    tag = 'v0.0.1-nightly.20261108.1'
    sha = 'a' * 40
    names = release.expected_asset_names('product', tag[1:], False) - {'release.json'}
    manifest = {'schema': release.SCHEMA_VERSION, 'app': 'product', 'tag': tag,
                'version': tag[1:], 'channel': 'nightly', 'source_commit': sha,
                'assets': [{'name': n, 'size': 1, 'sha256': 'b' * 64} for n in sorted(names)]}
    published = {'tag_name': tag, 'draft': False, 'prerelease': True,
                 'assets': [{'name': n, 'size': 1, 'digest': 'sha256:' + 'b' * 64}
                            for n in sorted(names | {'release.json'})]}
    assert verify(tag, manifest, published, sha)['source_sha'] == sha
    for value in ('0.0.1', '0.0.1-rc.1', '0.0.1-nightly.20261107.1'):
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
        try:
            verify(tag, manifest, bad, sha)
        except ValueError:
            pass
        else:
            raise AssertionError(field)
    for mutate in (lambda m: m['assets'].pop(),
                   lambda m: m.update(source_commit='c' * 40),
                   lambda m: m['assets'].append(m['assets'][0]),
                   lambda m: m['assets'][0].update(sha256='invalid')):
        bad = copy.deepcopy(manifest); mutate(bad)
        try:
            verify(tag, bad, published, sha)
        except ValueError:
            pass
        else:
            raise AssertionError('accepted invalid manifest')
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
        f'repos/{REPOSITORY}/compare/{sha}...main': json.dumps({'merge_base_commit': {'sha': sha}}),
    }
    with patch(__name__ + '.gh', side_effect=lambda *args: routes[args[-1]]):
        assert plan(tag)['source_sha'] == sha
        for route, bad in ((f'repos/{REPOSITORY}/releases/assets/1', content + ' '),
                           (f'repos/{REPOSITORY}/compare/{sha}...main',
                            json.dumps({'merge_base_commit': {'sha': 'e' * 40}}))):
            saved = routes[route]; routes[route] = bad
            try:
                plan(tag)
            except ValueError:
                pass
            else:
                raise AssertionError('accepted changed manifest or non-main source')
            routes[route] = saved
    with patch(__name__ + '.plan', return_value={'tag': tag, 'source_sha': sha}), \
            patch(__name__ + '.gh') as command:
        execute('plan', 'all', tag)
        command.assert_not_called()
        execute('release', 'app', tag)
        command.assert_called_once_with('workflow', 'run', 'deploy-services.yml', '--repo', REPOSITORY,
                                        '--ref', 'main', '-f', 'service=app', '-f', f'tag={tag}', '-f', 'apply=true')
    assert selection('all', tag) == ['discovery', 'relay', 'relay-cn', 'app']
    assert selection('www', 'www-v0.0.1') == ['www']
    for service, wrong_tag in (('www', tag), ('app', 'www-v0.0.1'), ('all', 'www-v0.0.1')):
        try:
            selection(service, wrong_tag)
        except ValueError:
            pass
        else:
            raise AssertionError('accepted wrong deployment group')
    scratch = release.ROOT / '.artifacts'
    scratch.mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(dir=scratch, prefix='deploy-check-') as temp:
        root = Path(temp)
        directory = root / 'vibeface-app'; (directory / 'web').mkdir(parents=True)
        (directory / 'web/_shell.html').write_text('released fixture')
        component = {'app': 'app', 'schema': 1, 'tag': tag, 'version': tag[1:],
                     'source_commit': sha, 'channel': 'nightly', 'target': None,
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
                       'channel': 'nightly', 'targets': ['app']}), \
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
