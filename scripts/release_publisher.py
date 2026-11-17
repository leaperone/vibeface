"""Exact source/CI gates, public completeness and immutable publication reconciliation."""
import hashlib
import json
from pathlib import Path
import re
import tempfile
import release_archive
import release_context
import release_format
import release_github
import release_inventory
import release_source


def require_ci(context):
    source = context['ci_source_commit']
    release_format.require(isinstance(source, str) and re.fullmatch(release_format.COMMIT, source), 'invalid CI source commit')
    batches = release_github.ci_runs(source)
    for candidate in (item for page in batches for item in page['workflow_runs']):
        if (candidate['head_sha'] == source and candidate['head_branch'] == 'main'
                and candidate['path'] == '.github/workflows/ci.yml' and candidate['event'] == 'push'
                and candidate['repository']['full_name'] == release_format.GITHUB_REPO
                and candidate['head_repository']['full_name'] == release_format.GITHUB_REPO
                and candidate['status'] == 'completed' and candidate['conclusion'] == 'success'):
            return candidate
    raise ValueError('release requires the exact successful main push CI of its source')


def plan(app, tag=None, require_main=False, publishing=False, github_output=None):
    info = release_context.identity(app)
    if tag:
        release_format.require(release_format.parse_tag(tag) == (app, info['version'], info['channel']), 'tag does not match selected application/version')
    if publishing or require_main:
        release_format.require(not release_context.run('git', 'status', '--porcelain'), 'release source must be clean')
        info.update(release_source.validate_source(info['source_commit'], app))
        require_ci(info)
        release_context.dependencies(app, required=True)
    if publishing:
        release_format.require(release_context.run('git', 'rev-parse', info['tag'] + '^{commit}') == info['source_commit'], 'publishing requires exact existing tag')
    if github_output:
        with open(github_output, 'a') as output:
            for key, value in info.items():
                output.write(f'{key}={value}\n')
    print(json.dumps(info))
    return info


def published(tag, pin=None, prove_source=True):
    app, value, channel = release_format.parse_tag(tag)
    record = release_github.github_json('releases/tags/' + tag)
    release_format.require(record['tag_name'] == tag and record['draft'] is False
            and record['prerelease'] == (channel != 'stable'), 'release publication identity differs')
    by_name = {asset['name']: asset for asset in record['assets']}
    release_format.require(len(by_name) == len(record['assets']) and 'release.json' in by_name, 'published complete manifest is required')
    marker = by_name['release.json']
    release_format.require(type(marker['size']) is int and 0 < marker['size'] <= 1024 * 1024, 'manifest exceeds limit')
    raw = release_github.asset_bytes(marker['id'])
    sha = hashlib.sha256(raw).hexdigest()
    release_format.require(len(raw) == marker['size'] and marker.get('digest') == 'sha256:' + sha, 'published manifest digest differs')
    info = release_format.verify_release_identity(json.loads(raw), release_context.contracts())
    release_format.require((info['app'], info['version'], info['channel'], info['tag']) == (app, value, channel, tag), 'published manifest tag differs')
    expected = {asset['name']: asset for asset in info['assets']}
    release_format.require(set(by_name) == set(expected) | {'release.json'}, 'release contains unexpected assets')
    signed = app == 'mobile' and f'vibeface-{value}-ios.ipa' in expected
    release_format.require(set(by_name) == release_format.expected_asset_names(app, value, signed), 'release is incomplete')
    for name, asset in expected.items():
        release_format.require(by_name[name]['size'] == asset['size'] and by_name[name].get('digest') == 'sha256:' + asset['sha256'], 'published asset metadata differs')
    if pin:
        release_format.validate_pin(app, pin)
        release_format.require(pin == {key: info[key] for key in ('tag', 'version', 'source_commit')} | {'manifest_sha256': sha}, 'published dependency pin differs')
    if prove_source:
        source = release_github.fetch_source(tag)
        release_format.require(source == info['source_commit'], 'published source differs from immutable tag')
        require_ci(release_source.validate_source(source, app))
    return info, record, sha


def dependency(app, dependency_app, target, output):
    pins = release_context.dependencies(app, required=True)
    release_format.require(dependency_app in pins, 'application does not own this dependency')
    pin = pins[dependency_app]
    info, _, _ = published(pin['tag'], pin)
    prefix = 'vibeface' if dependency_app == 'cli' else 'vibeface-' + dependency_app
    suffix = '-' + target if dependency_app in ('core', 'cli') else ''
    release_format.require(dependency_app not in ('core', 'cli') or target in release_format.TARGETS, 'native dependency requires a target')
    name = f'{prefix}-{pin["version"]}{suffix}.tar.gz'
    wanted = {name, name + '.sha256'}
    selected = [item for item in info['assets'] if item['name'] in wanted]
    release_format.require(len(selected) == 2, 'dependency lacks target payload/checksum')
    release_github.download_assets(pin['tag'], selected, output)
    release_archive.verify_checksum_file(Path(output), name)
    return info


def publish(app, directory, repo=release_format.GITHUB_REPO):
    release_format.require(repo == release_format.GITHUB_REPO, 'unexpected publication repository')
    expected = release_context.identity(app)
    plan(app, expected['tag'], require_main=True, publishing=True)
    info = release_inventory.verify_release(app, Path(directory), expected)
    tag = info['tag']
    view = release_github.release_view(tag, repo)
    if view.returncode:
        release_format.require('release not found' in view.stderr.lower(), 'cannot determine release state; reconcile before publishing')
        release_github.release_create(tag, repo, info['channel'] != 'stable')
        remote = {}
    else:
        record = json.loads(view.stdout)
        release_format.require(record['isDraft'] is False, 'release is still a draft')
        remote = {asset['name']: asset for asset in record['assets']}
        release_format.require(len(remote) == len(record['assets']), 'duplicate remote release assets')
    paths = [Path(directory) / asset['name'] for asset in info['assets']] + [Path(directory) / 'release.json']
    release_format.require(set(remote) <= {p.name for p in paths}, 'release has foreign assets')
    with tempfile.TemporaryDirectory() as temp:
        for path in paths:
            if path.name in remote:
                asset = remote[path.name]
                if asset.get('digest') is not None:
                    release_format.require(asset['size'] == path.stat().st_size and asset['digest'] == 'sha256:' + release_archive.digest(path), 'immutable published bytes differ; use a new version')
                else:
                    downloaded = release_github.download_assets(tag, [{'name': path.name, 'size': path.stat().st_size, 'sha256': release_archive.digest(path)}], Path(temp))
                    release_format.require(release_archive.digest(downloaded / path.name) == release_archive.digest(path), 'immutable release asset differs')
                continue
            # A write with an unknown result is never retried. The next invocation reconciles all uploaded bytes.
            release_github.release_upload(tag, repo, path)
    release_github.release_edit(tag, repo, f'{app} {info["version"]}; source {info["source_commit"]}.')
    return info


def verify_public(app, destination, repo=release_format.GITHUB_REPO):
    info, _, _ = published(release_context.identity(app)['tag'])
    release_format.require(info['app'] == app and repo == release_format.GITHUB_REPO, 'wrong public application')
    release_github.download_assets(info['tag'], info['assets'], destination)
    release_archive.write_json(Path(destination) / 'release.json', info)
    return release_inventory.verify_release(app, Path(destination), release_context.identity(app))
