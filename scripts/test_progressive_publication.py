#!/usr/bin/env python3
"""Controlled progressive publication tests with a fake gh transport."""
import hashlib
import json
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path
from unittest.mock import patch

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import release as rel  # noqa: E402


def require(condition, message):
    if not condition:
        raise AssertionError(message)


def digest(path):
    h = hashlib.sha256()
    with path.open('rb') as fh:
        for block in iter(lambda: fh.read(1024 * 1024), b''):
            h.update(block)
    return h.hexdigest()


def rejects(label, action, expect=None):
    try:
        action()
    except ValueError as error:
        if expect is not None:
            require(expect in str(error), f'{label}: {error}')
        return
    raise AssertionError(f'accepted {label}')


class FakeRelease:
    def __init__(self, tag, source):
        self.tag = tag
        self.source = source
        self.exists = False
        self.draft = False
        self.body = ''
        self.assets = {}
        self.uploads = []
        self.edits = []

    def asset_records(self):
        return [{'name': name, 'size': len(data), 'digest': f'sha256:{digest_bytes(data)}'}
                for name, data in sorted(self.assets.items())]

    def view_json(self):
        return json.dumps(dict(isDraft=self.draft, body=self.body,
                               assets=[{'name': name} for name in sorted(self.assets)]))

    def api_payload(self):
        return json.dumps(dict(assets=self.asset_records()))


def digest_bytes(data):
    return hashlib.sha256(data).hexdigest()


def gh_handler(state, calls):
    def runner(args, **kwargs):
        command = list(args)
        calls.append(command)
        if command[:2] == ['git', 'merge-base']:
            return subprocess.CompletedProcess(command, 0)
        if command[:2] == ['git', 'rev-parse']:
            if command[-1].endswith('^{commit}'):
                return subprocess.CompletedProcess(command, 0, state.source, '')
            if command[2] == 'HEAD':
                return subprocess.CompletedProcess(command, 0, state.source, '')
            return subprocess.CompletedProcess(command, 0, 'clean', '')
        if command[0] == 'git' and command[1] == 'status':
            return subprocess.CompletedProcess(command, 0, '', '')
        require(command[0] == 'gh', 'unexpected command')
        if command[1] == 'api':
            path = command[2]
            if path.endswith(f'/commits/{state.tag}'):
                return subprocess.CompletedProcess(command, 0, json.dumps(dict(sha=state.source)), '')
            if path.endswith(f'/releases/tags/{state.tag}'):
                require(state.exists, 'release missing')
                return subprocess.CompletedProcess(command, 0, state.api_payload(), '')
            raise AssertionError(path)
        action = command[2]
        if action == 'view':
            if not state.exists:
                return subprocess.CompletedProcess(command, 1, '', 'release not found')
            return subprocess.CompletedProcess(command, 0, state.view_json(), '')
        if action == 'create':
            state.exists = True
            state.draft = '--draft=false' not in command
            state.body = command[command.index('--notes') + 1]
            return subprocess.CompletedProcess(command, 0, '', '')
        if action == 'upload':
            for path in map(Path, command[5:]):
                state.uploads.append(path.name)
                state.assets[path.name] = path.read_bytes()
            return subprocess.CompletedProcess(command, 0, '', '')
        if action == 'download':
            name = command[command.index('--pattern') + 1]
            destination = Path(command[command.index('--dir') + 1])
            (destination / name).write_bytes(state.assets[name])
            return subprocess.CompletedProcess(command, 0, '', '')
        if action == 'edit':
            state.body = command[command.index('--notes') + 1]
            state.edits.append(command)
            return subprocess.CompletedProcess(command, 0, '', '')
        raise AssertionError(command)

    def runner_output(*args, **kwargs):
        command = list(args)
        calls.append(command)
        if command[:2] == ['git', 'rev-parse'] and len(command) > 2 and command[2] == 'HEAD':
            return state.source
        if command[:2] == ['git', 'status'] and command[2] == '--porcelain':
            return ''
        if len(command) >= 3 and command[:2] == ['gh', 'api'] and command[2].endswith(f'/releases/tags/{state.tag}'):
            require(state.exists, 'release missing')
            return state.api_payload()
        if len(command) >= 3 and command[:2] == ['gh', 'api'] and command[2].endswith(f'/commits/{state.tag}'):
            return json.dumps(dict(sha=state.source))
        if command[:2] == ['git', 'merge-base']:
            return ''
        raise AssertionError(command)

    return runner, runner_output


def build_app_part(tmp, version, source, channel, tag):
    part = tmp / 'app-part'
    part.mkdir()
    archive = part / f'vibeface-app-{version}.tar.gz'
    archive.write_bytes(b'app archive fixture')
    (part / (archive.name + '.sha256')).write_text(f'{digest(archive)}  {archive.name}\n', encoding='utf-8')
    rel.write_json(part / 'release.json', dict(
        app='product', version=version, tag=tag, source_commit=source, channel=channel, schema=rel.SCHEMA_VERSION,
        assets=[dict(name=archive.name, size=archive.stat().st_size, sha256=digest(archive))]))
    return part


def main():
    version = rel.canonical_product_version()
    tag = f'v{version}'
    source = 'a' * 40
    channel = rel.version(version)
    calls = []

    inventory = rel.progressive_part_inventory(version, ios_signed=False)
    require(len(inventory) == 11, 'eleven part groups')
    require(sum(len(names) for _, _, names in inventory) == 44, 'forty-four payloads')

    with tempfile.TemporaryDirectory() as temporary:
        tmp = Path(temporary)
        part = build_app_part(tmp, version, source, channel, tag)

        rejects('native part without target', lambda: rel.verify_part_directory('native', part, target=None),
                'require --target')
        rejects('app part with forbidden target',
                lambda: rel.verify_part_directory('app', part, target=rel.TARGETS[0]), 'forbidden')

        with patch.object(rel, 'identity', return_value=dict(app='product', version=version, tag=tag,
                                                             source_commit=source, channel=channel)):
            rel.verify_part_directory('app', part)
            corrupt = tmp / 'corrupt'
            shutil.copytree(part, corrupt)
            archive = corrupt / f'vibeface-app-{version}.tar.gz'
            archive.write_bytes(b'other bytes')
            rejects('checksum mismatch', lambda: rel.verify_part_directory('app', corrupt), 'checksum')

    state = FakeRelease(tag, source)
    handler, output = gh_handler(state, calls)
    part = build_app_part(Path(tempfile.mkdtemp()), version, source, channel, tag)
    app_names = rel.app_asset_names(version)

    def identity_patch(*args, **kwargs):
        return dict(app='product', version=version, tag=tag, source_commit=source, channel=channel)

    def publishing_patch():
        return identity_patch()

    with patch.object(rel, 'run', side_effect=output), patch(f'{rel.__name__}.subprocess.run', side_effect=handler):
        with patch.object(rel, 'require_product_publishing', side_effect=publishing_patch):
            with patch.object(rel, 'identity', side_effect=identity_patch):
                    rel.begin_product()
                    require(state.exists and not state.draft and rel.BUILDING_MARKER in state.body, 'building release')
                    rel.publish_part('app', part)
                    require(set(state.assets) == app_names, 'one part without manifest')
                    require('release.json' not in state.assets, 'no global manifest yet')
                    uploads = list(state.uploads)
                    same_calls = len(calls)
                    rel.publish_part('app', part)
                    require(state.uploads == uploads, 'equal remote bytes are a no-op')

                    conflict_dir = part.parent / 'conflict-part'
                    shutil.copytree(part, conflict_dir)
                    archive = conflict_dir / f'vibeface-app-{version}.tar.gz'
                    archive.write_bytes(b'different app payload')
                    (conflict_dir / (archive.name + '.sha256')).write_text(
                        f'{digest(archive)}  {archive.name}\n', encoding='utf-8')
                    try:
                        rel.publish_part('app', conflict_dir)
                    except ValueError as error:
                        require('existing published' in str(error), f'unexpected conflict error: {error}')
                    else:
                        raise AssertionError('accepted conflicting remote bytes')
                    require(len(state.uploads) == len(uploads), 'no blind second write after conflict')

    staged = part.parent / 'finalize-product'
    staged.mkdir()
    shutil.copy2(part / 'release.json', staged / 'release.json')
    fake_info = dict(app='product', version=version, tag=tag, source_commit=source, channel=channel,
                     schema=rel.SCHEMA_VERSION, assets=[])
    with patch.object(rel, 'run', side_effect=output), patch(f'{rel.__name__}.subprocess.run', side_effect=handler):
        with patch.object(rel, 'list_release_assets', return_value=state.asset_records()):
            rejects('finalize with partial remote',
                    lambda: rel.finalize_progressive_product(staged, identity_patch(), fake_info),
                    'cannot finalize before every payload is public')

    state.assets['release.json'] = b'{"app":"product"}\n'
    state.exists = True
    with patch.object(rel, 'run', side_effect=output), patch(f'{rel.__name__}.subprocess.run', side_effect=handler):
        with patch.object(rel, 'require_product_publishing', side_effect=publishing_patch):
            with patch.object(rel, 'identity', side_effect=identity_patch):
                rejects('immutability after manifest', rel.begin_product, 'immutable complete release')

    print('progressive publication tests passed')
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
