#!/usr/bin/env python3
"""Guard an existing official Node and inspect the process inside its container."""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import socket
import stat
import subprocess
import sys
import urllib.request


ROOT = Path('/opt/vibeface')
DATA = Path('/data')


def require(condition, message):
    if not condition:
        raise ValueError(message)


def regular(path, limit=1024 * 1024):
    with path.open('rb') as handle:
        metadata = os.fstat(handle.fileno())
        require(not path.is_symlink() and stat.S_ISREG(metadata.st_mode) and metadata.st_size <= limit,
                'expected a bounded regular file')
        return handle.read(limit + 1)


def component_info(directory=ROOT, content_root=None, verify=True, ignored=()):
    info = json.loads(regular(directory / 'COMPONENT-INFO.json'))
    app = info.get('app')
    fields = {'app', 'version', 'tag', 'source_commit', 'channel', 'schema', 'target', 'files'}
    require(app in ('core', 'app', 'www') and set(info) == fields | ({'data_epoch'} if app == 'core' else set()),
            'invalid component fields')
    require(type(info['schema']) is int and info['schema'] == 1
            and re.fullmatch(r'[0-9a-f]{40}', info['source_commit'])
            and re.fullmatch(r'\d+\.\d+\.\d+(?:-rc\.\d+|-nightly\.\d{8}\.\d+)?', info['version']),
            'invalid component identity')
    require(info['tag'] == ('www-v' if app == 'www' else 'v') + info['version'], 'component tag mismatch')
    if app == 'core':
        require(info['target'] == 'x86_64-unknown-linux-gnu' and type(info['data_epoch']) is int,
                'unsupported Core component')
    else:
        require(info['target'] is None, 'static component has a native target')
    prefix = '' if app == 'core' else ('web/' if app == 'app' else 'site/')
    expected = info['files']
    require(isinstance(expected, dict) and expected, 'component has no files')
    if app == 'core':
        require(set(expected) == {'vibeface-core'}, 'Core image must not contain the App')
    else:
        require(all(name.startswith(prefix) for name in expected), 'invalid static component layout')
    if verify:
        root = content_root or directory
        actual = {}
        for path in root.rglob('*'):
            require(not path.is_symlink(), 'component contains a symbolic link')
            if path.is_file():
                name = path.relative_to(root).as_posix()
                if not content_root and name in ('COMPONENT-INFO.json', *ignored):
                    continue
                actual[(prefix if content_root else '') + name] = hashlib.sha256(path.read_bytes()).hexdigest()
            else:
                require(path.is_dir(), 'component contains a special file')
        require(expected == actual, 'component files do not match the published manifest')
    return info


def image_info(verify=True):
    app = json.loads(regular(ROOT / 'COMPONENT-INFO.json'))['app']
    return component_info(ROOT, None if app == 'core' else Path('/usr/share/nginx/html'),
                          verify=verify, ignored=('runtime.py',))


def inspect():
    result = subprocess.run([str(ROOT / 'vibeface-core'), '--control'],
                            input=json.dumps({'command': 'inspect', 'data_dir': str(DATA)}),
                            text=True, capture_output=True, timeout=15)
    require(result.returncode == 0 and len(result.stdout) <= 1024 * 1024, 'Core inspect failed')
    response = json.loads(result.stdout)
    require(response.get('ok') is True and isinstance(response.get('result'), dict), 'Core inspect was refused')
    return response['result']


def guard(info):
    expected = os.environ
    metadata = DATA.lstat()
    require(stat.S_ISDIR(metadata.st_mode) and stat.S_IMODE(metadata.st_mode) == 0o700
            and metadata.st_uid == os.geteuid() and metadata.st_gid == os.getegid(),
            'existing private data directory ownership or permissions differ')
    for name in ('identity.json', 'data-epoch.json', 'config.json', 'node.lock'):
        path = DATA / name
        require(not path.is_symlink() and path.is_file(), 'initialized Node files are required')
    epoch = json.loads(regular(DATA / 'data-epoch.json', 4096))
    require(epoch == {'data_epoch': info['data_epoch']} and type(epoch['data_epoch']) is int,
            'data epoch differs; no migration or initialization is allowed')
    config = json.loads(regular(DATA / 'config.json'))
    role = expected['VIBEFACE_ROLE']
    network = config['network']
    require(role in ('discovery', 'entry', 'relay') and config['public_entry'] is True
            and config.get('manager') is None and config['roots'] == [] and network.get('entry') is None,
            'official infrastructure must run outside personal networks')
    require(config['listen'] == expected['VIBEFACE_LISTEN'], 'existing HTTP listen address differs')
    if role == 'discovery':
        require(network['relay_mode'] == 'discovery-only', 'Discovery role requires discovery-only mode')
    else:
        require(network.get('relay_mode', 'forward') == 'forward'
                and network['relay_admission']['policy'] == ('grants' if role == 'entry' else 'operator-token'),
                'relay admission does not match the selected role')
    require(info['version'] == expected['VIBEFACE_EXPECTED_VERSION']
            and info['source_commit'] == expected['VIBEFACE_EXPECTED_SOURCE'], 'image pin differs')
    result = inspect()
    require(result['device_id'] == expected['VIBEFACE_EXPECTED_DEVICE']
            and result['core_version'] == info['version'], 'existing Node identity or binary version differs')
    return result


def maintenance(command):
    endpoint = Path('/tmp') / ('vibeface-' + os.environ['VIBEFACE_EXPECTED_DEVICE'][:32]) / 'maintenance.sock'
    metadata = endpoint.lstat()
    parent = endpoint.parent.lstat()
    require(stat.S_ISSOCK(metadata.st_mode) and stat.S_IMODE(metadata.st_mode) == 0o600
            and metadata.st_uid == os.geteuid() and stat.S_ISDIR(parent.st_mode)
            and stat.S_IMODE(parent.st_mode) == 0o700 and parent.st_uid == os.geteuid(),
            'maintenance endpoint is not private')
    stream = socket.socket(socket.AF_UNIX)
    stream.settimeout(15)
    stream.connect(str(endpoint))
    stream.sendall((json.dumps({'protocol': 1, 'command': command}) + '\n').encode())
    response = bytearray()
    while not response.endswith(b'\n') and len(response) <= 4096:
        part = stream.recv(4096)
        require(part, 'maintenance response ended early')
        response.extend(part)
    value = json.loads(response)
    require(len(response) <= 4096 and value.get('protocol') == 1
            and value.get('status') == ('ready' if command == 'acquire' else 'healthy')
            and value['health']['device_id'] == os.environ['VIBEFACE_EXPECTED_DEVICE'],
            'maintenance lease refused or identity differs')
    return stream


def health(info):
    if info['app'] == 'core':
        result = inspect()
        require(result['status'] == 'running' and result['version'] == info['version']
                and result['device_id'] == os.environ['VIBEFACE_EXPECTED_DEVICE']
                and result['web_enabled'] == (os.environ['VIBEFACE_ROLE'] != 'discovery'),
                'Core health, version, identity or browser mode differs')
        return {key: result[key] for key in ('status', 'version', 'device_id')}
    request = urllib.request.Request('http://127.0.0.1:8080/')
    with urllib.request.build_opener(urllib.request.ProxyHandler({})).open(request, timeout=3) as response:
        require(response.status == 200 and response.headers.get('X-Vibeface-Source') == info['source_commit']
                and response.headers.get('X-Vibeface-Version') == info['version'], 'static health or image pin differs')
    return {'status': 'running', 'version': info['version'], 'source_commit': info['source_commit']}


def self_test():
    import tempfile
    from unittest.mock import patch
    artifact_root = Path(__file__).resolve().parent.parent / '.artifacts'
    artifact_root.mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(dir=artifact_root, prefix='runtime-check-') as temporary:
        directory = Path(temporary)
        (directory / 'vibeface-core').write_bytes(b'fixture')
        info = {'app': 'core', 'schema': 1, 'version': '1.0.0', 'tag': 'v1.0.0', 'channel': 'stable',
                'target': 'x86_64-unknown-linux-gnu', 'source_commit': 'a' * 40, 'data_epoch': 1,
                'files': {'vibeface-core': hashlib.sha256(b'fixture').hexdigest()}}
        (directory / 'COMPONENT-INFO.json').write_text(json.dumps(info))
        assert component_info(directory) == info
        data = directory / 'data'; data.mkdir(mode=0o700)
        for name in ('identity.json', 'node.lock'):
            (data / name).write_text('fixture')
        (data / 'data-epoch.json').write_text('{"data_epoch":1}')
        config = {'listen': '127.0.0.1:4900', 'public_entry': True, 'roots': [], 'manager': None,
                  'network': {'relay_mode': 'discovery-only', 'entry': None}}
        (data / 'config.json').write_text(json.dumps(config))
        env = {'VIBEFACE_ROLE': 'discovery', 'VIBEFACE_LISTEN': config['listen'],
               'VIBEFACE_EXPECTED_DEVICE': 'b' * 64, 'VIBEFACE_EXPECTED_VERSION': '1.0.0',
               'VIBEFACE_EXPECTED_SOURCE': 'a' * 40}
        module = sys.modules[__name__]
        with patch.object(module, 'DATA', data), patch.dict(os.environ, env), \
                patch.object(module, 'inspect', return_value={'device_id': 'b' * 64, 'core_version': '1.0.0'}):
            assert guard(info)['device_id'] == 'b' * 64
            for marker in ('{"data_epoch":2}', '{"data_epoch":true}'):
                (data / 'data-epoch.json').write_text(marker)
                try:
                    guard(info)
                except ValueError:
                    pass
                else:
                    raise AssertionError('different epoch accepted')
            (data / 'data-epoch.json').write_text('{"data_epoch":1}')
            (data / 'identity.json').unlink()
            try:
                guard(info)
            except ValueError:
                pass
            else:
                raise AssertionError('missing identity accepted')
            assert not (data / 'identity.json').exists()
    print('container runtime self-test passed')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('command', choices=('info', 'guard', 'start', 'health', 'lease', 'self-test'))
    args = parser.parse_args()
    if args.command == 'self-test':
        self_test(); return
    info = image_info(verify=args.command not in ('health', 'lease'))
    if args.command == 'info':
        print(json.dumps({key: value for key, value in info.items() if key != 'files'}))
    elif args.command == 'health':
        print(json.dumps(health(info)))
    elif args.command in ('guard', 'start'):
        result = guard(info)
        if args.command == 'guard':
            print(json.dumps({'device_id': result['device_id'], 'version': info['version']}))
        else:
            with (DATA / 'node.lock').open('rb') as lock:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            args = [str(ROOT / 'vibeface-core'), '--data-dir', str(DATA)]
            if os.environ['VIBEFACE_ROLE'] == 'discovery':
                args.append('--no-web')
            os.execv(args[0], args)
    else:
        with maintenance('acquire'):
            print(json.dumps({'status': 'ready'}), flush=True)
            sys.stdin.buffer.read(1)


if __name__ == '__main__':
    try:
        main()
    except (ValueError, KeyError, TypeError, OSError, subprocess.SubprocessError) as error:
        sys.exit('container validation failed: ' + type(error).__name__)
