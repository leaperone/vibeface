#!/usr/bin/env python3
"""Controlled progressive publication tests with a fake gh transport."""
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path
from unittest.mock import patch

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import release as rel  # noqa: E402
from progressive_publication_fixtures import build_collected_tree  # noqa: E402


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
    except (ValueError, OSError) as error:
        if expect is not None:
            require(expect in str(error), f'{label}: {error}')
        return
    except Exception as error:
        if expect is not None and expect in str(error):
            return
        raise
    raise AssertionError(f'accepted {label}')


class FakeRelease:
    def __init__(self, tag, source):
        self.tag = tag
        self.source = source
        self.exists = False
        self.draft = False
        self.body = ''
        self.immutable = False
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
        return json.dumps(dict(assets=self.asset_records(), immutable=self.immutable))


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


def build_minimal_app_part(tmp, version, source, channel, tag):
    part = tmp / 'minimal-app'
    part.mkdir()
    archive = part / f'vibeface-app-{version}.tar.gz'
    archive.write_bytes(b'app archive fixture')
    (part / (archive.name + '.sha256')).write_text(f'{digest(archive)}  {archive.name}\n', encoding='utf-8')
    rel.write_json(part / 'release.json', dict(
        app='product', version=version, tag=tag, source_commit=source, channel=channel, schema=rel.SCHEMA_VERSION,
        assets=[dict(name=archive.name, size=archive.stat().st_size, sha256=digest(archive))]))
    return part


def install_fake_transport(bin_dir, state_path):
    bin_dir.mkdir(parents=True, exist_ok=True)
    helper = HERE / 'progressive_publication_fake_gh.py'
    for role in ('gh', 'git'):
        script = bin_dir / role
        script.write_text(
            f'#!/usr/bin/env python3\nimport os, runpy\nos.environ["VIBEFACE_TEST_GH_STATE"] = {str(state_path)!r}\n'
            f'os.environ["VIBEFACE_TEST_TRANSPORT_ROLE"] = {role!r}\n'
            f'runpy.run_path({str(helper)!r}, run_name="__main__")\n',
            encoding='utf-8')
        script.chmod(0o755)


def test_real_validators():
    collected, product = build_collected_tree()
    target = rel.TARGETS[0]
    desktop = rel.DESKTOP_TARGETS[0]
    with patch.object(rel, 'identity', return_value=product):
        rel.verify_part_directory('app', collected / 'app')
        rel.verify_part_directory('native', collected / 'native' / target, target=target)
        rel.verify_part_directory('desktop', collected / 'desktop' / desktop, target=desktop)
        native_dir = collected / 'native' / target
        core = native_dir / f'vibeface-core-{product["version"]}-{target}.tar.gz'
        bad = native_dir.parent / 'bad-native' / target
        bad.mkdir(parents=True)
        for name in native_dir.iterdir():
            if name.is_file():
                shutil.copy2(name, bad / name.name)
        bundle = bad / f'vibeface-{product["version"]}-{target}.tar.gz'
        import io
        import tarfile
        with tarfile.open(bundle, 'w:gz') as archive:
            payload = json.dumps(dict(version=product['version'], target=target, source_commit='b' * 40,
                                      update_protocol=1, data_epoch=1, core_version=product['version'],
                                      core_source_commit='b' * 40, app_version=product['version'],
                                      app_source_commit='b' * 40)).encode()
            info = tarfile.TarInfo('evil/BUILD-INFO.json')
            info.size = len(payload)
            archive.addfile(info, io.BytesIO(payload))
        (bad / (bundle.name + '.sha256')).write_text(f'{digest(bundle)}  {bundle.name}\n', encoding='utf-8')
        rejects('wrong native CLI bundle', lambda: rel.verify_part_directory('native', bad, target=target),
                'CLI bundle version')


def test_cli_publish_part_json():
    source = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=HERE.parent, text=True).strip()
    collected, product = build_collected_tree(source)
    version = product['version']
    tag = product['tag']
    with tempfile.TemporaryDirectory() as temporary:
        bin_dir = Path(temporary) / 'bin'
        bin_dir.mkdir()
        state_path = Path(temporary) / 'gh-state.json'
        state_path.write_text(json.dumps(dict(tag=tag, source=source, exists=True, draft=False,
                                                body=rel.BUILDING_MARKER, assets={}), indent=2), encoding='utf-8')
        install_fake_transport(bin_dir, state_path)
        env = os.environ.copy()
        env['PATH'] = f'{bin_dir}{os.pathsep}{env.get("PATH", "")}'
        env['VIBEFACE_TEST_GH_STATE'] = str(state_path)
        part = collected / 'app'
        proc = subprocess.run(
            [sys.executable, '-B', str(HERE / 'release.py'), 'publish-part', '--kind', 'app', '--input', str(part)],
            cwd=str(HERE.parent), capture_output=True, text=True, env=env)
        require(proc.returncode == 0, proc.stderr)
        summary = json.loads(proc.stdout)
        require(isinstance(summary['names'], list) and 'frozenset' not in proc.stdout, 'serializable CLI output')
        require(set(summary['names']) == rel.app_asset_names(version), 'app payload names in summary')


def main():
    version = rel.canonical_product_version()
    tag = f'v{version}'
    source = 'a' * 40
    channel = rel.version(version)
    calls = []

    require('manual verified platform downloads' in rel.product_building_notes(dict(version=version, source_commit=source)),
            'building notes')

    inventory = rel.progressive_part_inventory(version, ios_signed=False)
    require(len(inventory) == 11, 'eleven part groups')
    require(sum(len(names) for _, _, names in inventory) == 44, 'forty-four payloads')

    test_real_validators()
    test_cli_publish_part_json()

    with tempfile.TemporaryDirectory() as temporary:
        tmp = Path(temporary)
        part = build_minimal_app_part(tmp, version, source, channel, tag)

        rejects('native part without target', lambda: rel.verify_part_directory('native', part, target=None),
                'require --target')
        rejects('app part with forbidden target',
                lambda: rel.verify_part_directory('app', part, target=rel.TARGETS[0]), 'forbidden')
        identity = dict(app='product', version=version, tag=tag, source_commit=source, channel=channel)
        with patch.object(rel, 'identity', return_value=identity):
            corrupt = tmp / 'corrupt'
            shutil.copytree(part, corrupt)
            archive = corrupt / f'vibeface-app-{version}.tar.gz'
            archive.write_bytes(b'other bytes')
            rejects('checksum mismatch', lambda: rel.verify_part_directory('app', corrupt), 'checksum')

    state = FakeRelease(tag, source)
    handler, output = gh_handler(state, calls)
    collected, product = build_collected_tree(source)
    part = collected / 'app'
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
                summary = rel.publish_part('app', part)
                json.dumps(summary)
                require(set(state.assets) == app_names, 'one part without manifest')
                require('release.json' not in state.assets, 'no global manifest yet')
                uploads = list(state.uploads)
                same_calls = len(calls)
                rel.publish_part('app', part)
                require(state.uploads == uploads, 'equal remote bytes are a no-op')
                require(isinstance(summary['names'], list), 'public summary uses list names')

                archive_name = next(iter(app_names))
                state.assets[archive_name] = b'different app payload on the remote'
                try:
                    rel.publish_part('app', part)
                except ValueError as error:
                    require('existing published' in str(error), f'unexpected conflict error: {error}')
                else:
                    raise AssertionError('accepted conflicting remote bytes')
                require(len(state.uploads) == len(uploads), 'no blind second write after conflict')

                desktop = rel.DESKTOP_TARGETS[0]
                rel.publish_part('desktop', collected / 'desktop' / desktop, target=desktop)
                require(any(name.startswith('vibeface-desktop-') for name in state.assets), 'desktop part uploaded')

    staged = part.parent / 'finalize-product'
    staged.mkdir()
    shutil.copy2(collected / 'app' / 'release.json', staged / 'release.json')
    fake_info = dict(app='product', version=version, tag=tag, source_commit=source, channel=channel,
                     schema=rel.SCHEMA_VERSION, assets=[])
    with patch.object(rel, 'run', side_effect=output), patch(f'{rel.__name__}.subprocess.run', side_effect=handler):
        with patch.object(rel, 'list_release_assets', return_value=state.asset_records()):
            rejects('finalize with partial remote',
                    lambda: rel.finalize_progressive_product(staged, identity_patch(), fake_info),
                    'cannot finalize before every payload is public')

    state.assets['release.json'] = json.dumps(dict(app='product', version=version, tag=tag, source_commit=source,
                                                    channel=channel, schema=rel.SCHEMA_VERSION, assets=[]),
                                              indent=2).encode() + b'\n'
    with patch.object(rel, 'run', side_effect=output), patch(f'{rel.__name__}.subprocess.run', side_effect=handler):
        with patch.object(rel, 'require_product_publishing', side_effect=publishing_patch):
            with patch.object(rel, 'identity', side_effect=identity_patch):
                rejects('partial manifest on public release', rel.begin_product, 'immutable complete release')

    state.immutable = True
    with patch.object(rel, 'run', side_effect=output), patch(f'{rel.__name__}.subprocess.run', side_effect=handler):
        with patch.object(rel, 'require_product_publishing', side_effect=publishing_patch):
            with patch.object(rel, 'identity', side_effect=identity_patch):
                rejects('immutable release', rel.begin_product, 'immutable')

    rejects('duplicate remote asset names', lambda: rel.remote_assets_by_name([
        dict(name='a.bin', size=1, digest='sha256:' + '0' * 64),
        dict(name='a.bin', size=2, digest='sha256:' + '1' * 64),
    ]), 'duplicate')

    print('progressive publication tests passed')
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
