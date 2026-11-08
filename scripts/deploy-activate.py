#!/usr/bin/env python3
"""Activate an already imported official service image without building or initializing data."""
import argparse
from contextlib import contextmanager
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import select
import shutil
import socket
import stat
import struct
import subprocess
import sys
import tarfile
import time
import uuid


CORE_ROLES = ('discovery', 'entry', 'relay')
ROLES = (*CORE_ROLES, 'app', 'www')
IMAGE = r'sha256:[0-9a-f]{64}'
VERSION = r'(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)(?:-rc\.[1-9][0-9]*|-nightly\.[0-9]{8}\.[1-9][0-9]*)?'
DOCKER = ['docker', '--host', 'unix:///var/run/docker.sock']
HEADER_NAMES = {'content-security-policy', 'x-content-type-options', 'x-frame-options',
                'referrer-policy', 'cache-control', 'x-vibeface-source', 'x-vibeface-version'}
STATIC_REGEX = r'\.(?:js|wasm|css|woff2?|svg|png|ico|json|webmanifest)$'


def require(condition, message):
    if not condition:
        raise ValueError(message)


def canonical_path(value):
    require(isinstance(value, str) and value.startswith('/') and str(Path(value)) == value
            and not any(part in ('.', '..') or 'trash' in part.lower() for part in Path(value).parts)
            and not any(c in value for c in '\r\n\x00,'), 'invalid absolute deployment path')
    return Path(value)


def validate_config(config):
    common = {'project', 'role', 'deploy_dir', 'listen'}
    require(isinstance(config, dict) and config.get('role') in ROLES, 'invalid service role')
    required = common | ({'data_dir', 'device_id', 'uid', 'gid', 'legacy_service', 'maintenance_socket'}
                         if config['role'] in CORE_ROLES else {'nginx_config', 'server_name'})
    if config['role'] == 'relay':
        required |= {'nginx_config', 'server_name'}
    require(required <= config.keys() and config.keys() <= required | {'public_url'},
            'unsupported or missing deployment configuration fields')
    require(re.fullmatch(r'[a-z][a-z0-9-]{0,47}', config['project']), 'invalid Compose project')
    require(re.fullmatch(r'127\.0\.0\.1:[1-9][0-9]{0,4}', config['listen'])
            and int(config['listen'].split(':')[1]) <= 65535, 'listen must be an explicit IPv4 loopback port')
    for name in ('deploy_dir', 'data_dir', 'nginx_config', 'maintenance_socket'):
        if name in config:
            canonical_path(config[name])
    if config['role'] in CORE_ROLES:
        require(re.fullmatch(r'[0-9a-f]{64}', config['device_id'])
                and all(type(config[n]) is int and 0 < config[n] <= 2**31 - 1 for n in ('uid', 'gid'))
                and re.fullmatch(r'[a-zA-Z0-9][a-zA-Z0-9_.@-]{0,127}\.service', config['legacy_service']),
                'invalid existing service identity')
        data, deploy = Path(config['data_dir']), Path(config['deploy_dir'])
        require(data != deploy and data not in deploy.parents and deploy not in data.parents,
                'deployment files and Node data must be separate')
    if 'nginx_config' in config:
        require(re.fullmatch(r'[a-zA-Z0-9][a-zA-Z0-9.-]{0,252}', config['server_name']), 'invalid nginx server name')
    if 'public_url' in config:
        require(re.fullmatch(r'https://[a-zA-Z0-9][a-zA-Z0-9.:-]*(?::[0-9]+)?', config['public_url']),
                'public_url must be an HTTPS origin without credentials')
    return config


def checked_path(path, missing=False):
    for part in reversed((path, *path.parents)):
        try:
            metadata = part.lstat()
        except FileNotFoundError:
            require(missing and part == path, 'deployment path does not exist')
            continue
        require(not stat.S_ISLNK(metadata.st_mode), 'deployment paths must not traverse symbolic links')
    return path


def read_json(path):
    checked_path(path)
    require(path.is_file() and path.stat().st_size <= 1024 * 1024, 'invalid state file')
    return json.loads(path.read_text())


def atomic_write(path, data, metadata=None):
    checked_path(path, missing=True)
    temporary = path.with_name('.' + path.name + '.' + uuid.uuid4().hex)
    try:
        with temporary.open('xb') as output:
            os.fchmod(output.fileno(), stat.S_IMODE(metadata.st_mode) if metadata else 0o600)
            if metadata:
                os.fchown(output.fileno(), metadata.st_uid, metadata.st_gid)
            output.write(data)
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, path)
        with open_directory(path.parent) as descriptor:
            os.fsync(descriptor)
    finally:
        temporary.unlink(missing_ok=True)


@contextmanager
def open_directory(path):
    descriptor = os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        yield descriptor
    finally:
        os.close(descriptor)


def write_json(path, value):
    atomic_write(path, (json.dumps(value, sort_keys=True, indent=2) + '\n').encode())


def command(*args, timeout=120, check=True):
    result = subprocess.run(args, capture_output=True, text=True, timeout=timeout)
    require(len(result.stdout) <= 2 * 1024 * 1024, 'external command response is too large')
    if check:
        require(result.returncode == 0, 'external command failed: ' + Path(args[0]).name)
    return result


def docker(*args, **kwargs):
    return command(*DOCKER, *args, **kwargs).stdout.strip()


def compose(manifest, *args):
    directory = Path(manifest['directory'])
    return docker('compose', '--project-name', manifest['project'], '--env-file', str(directory / 'image.env'),
                  '-f', str(directory / 'compose.yaml'), '-f', str(directory / 'runtime.json'), *args)


def containers(manifest):
    ids = compose(manifest, 'ps', '--all', '--quiet', 'service').split()
    require(all(re.fullmatch(r'[0-9a-f]{64}', value) for value in ids) and len(ids) <= 1,
            'unexpected Compose container set')
    return ids


def container_info(manifest):
    ids = containers(manifest)
    require(len(ids) == 1, 'deployment has no unique container')
    info = json.loads(docker('container', 'inspect', ids[0]))[0]
    labels = info['Config']['Labels']
    require(labels.get('com.docker.compose.project') == manifest['project']
            and labels.get('com.docker.compose.service') == 'service'
            and labels.get('dev.vibeface.deployment') == manifest['fingerprint']
            and info['Image'] == manifest['image'], 'container ownership or deployment configuration differs')
    return ids[0], info


def healthy(manifest):
    container, info = container_info(manifest)
    require(info['Image'] == manifest['image'] and info['State']['Status'] == 'running'
            and info['State'].get('Health', {}).get('Status') == 'healthy', 'container is not healthy at the pinned image')
    result = json.loads(docker('exec', container, 'python3', '/opt/vibeface/runtime.py', 'health'))
    require(result['status'] == 'running' and result['version'] == manifest['version'], 'running version differs')
    if manifest['role'] in CORE_ROLES:
        require(result['device_id'] == manifest['device_id'], 'running identity differs')
    else:
        require(result['source_commit'] == manifest['source_sha'], 'static source differs')
    return container


def wait_healthy(manifest):
    for _ in range(45):
        try:
            return healthy(manifest)
        except (ValueError, KeyError, json.JSONDecodeError):
            time.sleep(2)
    raise ValueError('new container did not become healthy')


def stopped(manifest):
    for container in containers(manifest):
        info = json.loads(docker('container', 'inspect', container))[0]['State']
        require(not info['Running'] and info['Pid'] == 0, 'previous process must not start while the new one is running')


def stop(manifest):
    if containers(manifest):
        container_info(manifest)
    compose(manifest, 'stop', '--timeout', '90', 'service')
    stopped(manifest)


def core_environment(config, version, source):
    return {'VIBEFACE_ROLE': config['role'], 'VIBEFACE_EXPECTED_DEVICE': config['device_id'],
            'VIBEFACE_EXPECTED_VERSION': version, 'VIBEFACE_EXPECTED_SOURCE': source,
            'VIBEFACE_LISTEN': config['listen']}


def compose_runtime(config, image, version, source, fingerprint):
    service = {'image': image, 'labels': {'dev.vibeface.deployment': fingerprint}, 'tmpfs': ['/tmp:rw,nosuid,noexec,size=64m,mode=1777']}
    if config['role'] in CORE_ROLES:
        service.update(network_mode='host', user=f'{config["uid"]}:{config["gid"]}',
                       environment=core_environment(config, version, source),
                       volumes=[{'type': 'bind', 'source': config['data_dir'], 'target': '/data',
                                 'bind': {'create_host_path': False}}])
    else:
        service.update(user='101:101', ports=[{'target': 8080, 'published': config['listen'].split(':')[1],
                                             'host_ip': '127.0.0.1', 'protocol': 'tcp'}])
    return {'services': {'service': service}}


def image_preflight(config, image, tag, source, version):
    info = json.loads(docker('image', 'inspect', image))[0]
    require(info['Id'] == image and info['Os'] == 'linux' and info['Architecture'] == 'amd64', 'image identity or platform differs')
    component = json.loads(docker('run', '--rm', '--network', 'none', '--read-only', '--entrypoint', 'python3',
                                  image, '/opt/vibeface/runtime.py', 'info'))
    require(component['app'] == ('core' if config['role'] in CORE_ROLES else config['role'])
            and component['version'] == version and component['tag'] == tag and component['source_commit'] == source,
            'image component differs from the selected published release')
    if config['role'] in CORE_ROLES:
        data = checked_path(Path(config['data_dir']))
        metadata = data.stat()
        require(data.is_dir() and stat.S_IMODE(metadata.st_mode) == 0o700
                and metadata.st_uid == config['uid'] and metadata.st_gid == config['gid'],
                'existing data ownership or permissions differ')
        for name in ('identity.json', 'data-epoch.json', 'config.json', 'node.lock'):
            checked_path(data / name)
            require((data / name).is_file(), 'initialized Node files are required')
        epoch = read_json(data / 'data-epoch.json')
        require(epoch == {'data_epoch': component['data_epoch']} and type(epoch['data_epoch']) is int,
                'data epoch differs; migration is not allowed')
        options = ['--user', f'{config["uid"]}:{config["gid"]}', '--tmpfs', '/tmp:rw,nosuid,noexec,mode=1777',
                   '--mount', f'type=bind,src={data},dst=/data,readonly']
        for key, value in core_environment(config, version, source).items():
            options.extend(('--env', key + '=' + value))
        docker('run', '--rm', '--network', 'none', '--read-only', *options, image, 'guard')
        if 'public_url' in config:
            require(read_json(data / 'config.json')['public_url'] == config['public_url'], 'existing public URL differs')
    return component


def nginx_nodes(text):
    tokens = []
    pattern = re.compile(r'''\s+|\#[^\n]*|"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|[{};]|[^\s{};\#'"]+''')
    position = 0
    for match in pattern.finditer(text):
        require(match.start() == position, 'unsupported nginx syntax')
        position = match.end()
        if not match[0].isspace() and not match[0].startswith('#'):
            tokens.append((match[0], match.start(), match.end()))
    require(position == len(text), 'unsupported nginx syntax')
    def parse(index, nested):
        result = []
        while index < len(tokens):
            if tokens[index][0] == '}':
                require(nested, 'unexpected nginx block end')
                return result, index + 1
            start = tokens[index][1]
            words = []
            while index < len(tokens) and tokens[index][0] not in ('{', '}', ';'):
                words.append(tokens[index][0]); index += 1
            require(words and index < len(tokens) and tokens[index][0] != '}', 'invalid nginx directive')
            if tokens[index][0] == '{':
                children, index = parse(index + 1, True)
            else:
                children = None; index += 1
            result.append({'words': words, 'children': children, 'start': start, 'end': tokens[index - 1][2]})
        require(not nested, 'unclosed nginx block')
        return result, index
    return parse(0, False)[0]


def transform_nginx(text, config):
    servers = [node for node in nginx_nodes(text) if node['words'] == ['server'] and node['children']]
    targets = [server for server in servers
               if any(n['words'][0] == 'server_name' and config['server_name'] in n['words'][1:] for n in server['children'])
               and any(n['words'][0] == 'listen' and 'ssl' in n['words'][1:] for n in server['children'])]
    require(len(targets) == 1, 'nginx config must contain one matching TLS server')
    edits = []
    catchall = 0
    relay_routes = 0
    for node in targets[0]['children']:
        words = node['words']
        if words[0] != 'location':
            if words[0] in ('root', 'index') or config['role'] in ('app', 'www') and (words[0] == 'add_header' and len(words) >= 3 and words[1].lower() in HEADER_NAMES):
                edits.append((node['start'], node['end'], ''))
            continue
        selector = ' '.join(words[1:])
        relay_routes += int(words[-1].strip('"\'') == '^/(relay|ping|generate_204)$')
        is_catchall = words == ['location', '/']
        catchall += int(is_catchall)
        if config['role'] == 'relay':
            require(is_catchall or words == ['location', '~', '^/(relay|ping|generate_204)$'],
                    'pure Relay refuses extra frontend or API routes')
            if not is_catchall:
                continue
            body = 'return 404;'
        else:
            is_static = is_catchall or (len(words) == 3 and words[1] in ('=', '^~') and words[2] in (
                '/assets/', '/generated/', '/manifest.webmanifest', '/vibeface_secure_bg.wasm',
                '/vibeface_secure.js', '/_shell.html', '/index.html', '/icon.svg', '/sw.js')) \
                or len(words) == 3 and words[1] in ('~', '~*') and words[2] == STATIC_REGEX
            directives = {n['words'][0] for n in node['children'] or []}
            require('try_files' not in directives or is_static, 'unrecognized static nginx location')
            if not is_static:
                infra = (len(words) == 3 and words[1] == '=' and words[2] in
                         ('/api/info', '/api/client', '/api/relay/grant', '/api/decision')) \
                        or words == ['location', '^~', '/bootstrap/'] \
                        or words == ['location', '~', '^/(relay|ping|generate_204)$']
                denied = all(n['words'] == ['return', '404'] for n in node['children'] or [])
                require((infra and not directives & {'try_files', 'alias', 'root', 'index', 'include'}) or denied,
                        'unrecognized static or gateway nginx location')
                continue
            require(directives <= {'try_files', 'types', 'default_type', 'add_header', 'expires', 'access_log',
                                   'root', 'index', 'proxy_pass', 'proxy_set_header', 'proxy_http_version',
                                   'proxy_intercept_errors', 'proxy_cache'}, 'unsupported static nginx directives')
            body = ('proxy_pass http://' + config['listen'] + ';\n'
                    '        proxy_http_version 1.1;\n        proxy_set_header Host $host;\n'
                    '        proxy_set_header Connection "";\n        proxy_intercept_errors off;\n        proxy_cache off;')
        indent = text[text.rfind('\n', 0, node['start']) + 1:node['start']]
        replacement = 'location ' + selector + ' {\n' + indent + '    ' + body + '\n' + indent + '}'
        edits.append((node['start'], node['end'], replacement))
    require(catchall == 1 and (config['role'] != 'relay' or relay_routes == 1), 'expected nginx routes are missing')
    for start, end, replacement in sorted(edits, reverse=True):
        text = text[:start] + replacement + text[end:]
    nginx_nodes(text)
    return text


@contextmanager
def lease(config, previous):
    if previous:
        container = healthy(previous)
        process = subprocess.Popen([*DOCKER, 'exec', '-i', container, 'python3', '/opt/vibeface/runtime.py', 'lease'],
                                   stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        try:
            require(select.select([process.stdout], [], [], 20)[0], 'maintenance lease response timed out')
            response = process.stdout.readline(4097)
            require(len(response) <= 4096 and json.loads(response) == {'status': 'ready'}, 'maintenance lease refused')
            yield
        finally:
            process.stdin.close()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.terminate()
                process.wait(timeout=5)
    else:
        endpoint = checked_path(Path(config['maintenance_socket']))
        metadata, parent = endpoint.lstat(), endpoint.parent.lstat()
        require(stat.S_ISSOCK(metadata.st_mode) and metadata.st_uid == config['uid']
                and stat.S_IMODE(metadata.st_mode) == 0o600 and parent.st_uid == config['uid']
                and stat.S_IMODE(parent.st_mode) == 0o700, 'legacy maintenance socket is not private')
        stream = socket.socket(socket.AF_UNIX)
        stream.settimeout(15)
        try:
            os.setegid(config['gid']); os.seteuid(config['uid'])
            stream.connect(str(endpoint))
        finally:
            os.seteuid(0); os.setegid(0)
        try:
            _, uid, _ = struct.unpack('3i', stream.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))
            require(uid == config['uid'], 'legacy maintenance peer differs')
            stream.sendall(b'{"protocol":1,"command":"acquire"}\n')
            packet = bytearray()
            while not packet.endswith(b'\n') and len(packet) <= 4096:
                part = stream.recv(4096)
                require(part, 'legacy maintenance connection ended early')
                packet.extend(part)
            value = json.loads(packet)
            require(len(packet) <= 4096 and value.get('protocol') == 1 and value.get('status') == 'ready'
                    and value['health']['device_id'] == config['device_id'], 'legacy maintenance lease was refused')
            yield
        finally:
            stream.close()


@contextmanager
def node_lock(config):
    path = checked_path(Path(config['data_dir']) / 'node.lock')
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    try:
        fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
        yield
    finally:
        os.close(descriptor)


def backup_data(config, destination):
    data = Path(config['data_dir'])
    with tarfile.open(destination, 'x:gz', dereference=False) as archive:
        for root, directories, names in os.walk(data, followlinks=False):
            for name in [*directories, *names]:
                path = Path(root) / name
                mode = path.lstat().st_mode
                require(stat.S_ISDIR(mode) or stat.S_ISREG(mode), 'offline backup refuses links and special files')
            archive.add(root, arcname=str(Path(root).relative_to(data)) or '.', recursive=False)
            for name in sorted(names):
                archive.add(Path(root) / name, arcname=str((Path(root) / name).relative_to(data)), recursive=False)
    os.chmod(destination, 0o600)
    with destination.open('rb') as handle:
        os.fsync(handle.fileno())


def nginx_apply(config, data, metadata):
    path = Path(config['nginx_config'])
    atomic_write(path, data, metadata)
    command('nginx', '-t')
    command('systemctl', 'reload', 'nginx.service')


def restored_previous(previous):
    if not previous:
        return False
    try:
        _, info = container_info(previous)
    except (ValueError, KeyError, json.JSONDecodeError):
        return False
    if not info['State']['Running']:
        return False
    wait_healthy(previous)
    return True


def rollback(journal, journal_path):
    target, config = journal['target'], journal['config']
    phase = journal['phase']
    if phase == 'prepared':
        journal['phase'] = 'rolled_back'
        write_json(journal_path, journal)
        return
    previous = journal['previous']
    restored = restored_previous(previous)
    if phase != 'stopping_previous' and not restored:
        stop(target)
    if phase == 'switching_nginx' and journal.get('nginx_backup'):
        path = checked_path(Path(config['nginx_config']))
        nginx_apply(config, Path(journal['nginx_backup']).read_bytes(), path.stat())
    if config['role'] in CORE_ROLES and not restored:
        with node_lock(config):
            require(hashlib.sha256((Path(config['data_dir']) / 'identity.json').read_bytes()).hexdigest()
                    == journal['identity_sha256'], 'identity changed; automatic rollback was refused')
        if previous:
            compose(previous, 'up', '-d', '--no-build', '--pull', 'never', 'service')
            wait_healthy(previous)
        else:
            command('systemctl', 'start', config['legacy_service'])
            if journal['legacy_enabled']:
                command('systemctl', 'enable', config['legacy_service'])
            command('systemctl', 'is-active', '--quiet', config['legacy_service'])
    elif previous and not restored:
        compose(previous, 'up', '-d', '--no-build', '--pull', 'never', 'service')
        wait_healthy(previous)
    current_path = journal_path.parent / 'current.json'
    if previous:
        write_json(current_path, previous)
    else:
        current_path.unlink(missing_ok=True)
    journal['phase'] = 'rolled_back'
    write_json(journal_path, journal)


def nginx_matches(config, manifest):
    if 'nginx_config' in config:
        nginx = checked_path(Path(config['nginx_config']))
        require(hashlib.sha256(nginx.read_bytes()).hexdigest() == manifest['nginx_sha256'],
                'active nginx configuration differs; reconcile before retrying')


def activate(config, image, tag, source, version):
    require(os.geteuid() == 0 and sys.platform == 'linux', 'activation requires root on the target Linux host')
    directory = checked_path(Path(config['deploy_dir']), missing=True)
    directory.mkdir(mode=0o700, exist_ok=True)
    require(directory.stat().st_uid == 0 and stat.S_IMODE(directory.stat().st_mode) == 0o700,
            'deployment directory must be private and root owned')
    descriptor = os.open('/run/lock/vibeface-deploy.lock', os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    try:
        fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
        require(os.fstat(descriptor).st_uid == 0, 'deployment lock owner differs')
        image_preflight(config, image, tag, source, version)
        current_path, journal_path = directory / 'current.json', directory / 'activation.json'
        current = read_json(current_path) if current_path.exists() else None
        fingerprint = hashlib.sha256(json.dumps([config, image, tag, source, version], sort_keys=True).encode()).hexdigest()
        if journal_path.exists():
            pending = read_json(journal_path)
            require(pending.get('schema') == 1, 'unsupported activation journal')
            if pending['phase'] not in ('confirmed', 'rolled_back'):
                rollback(pending, journal_path)
                raise ValueError('interrupted activation was rolled back; rerun after checking the previous service')
        if current and current['fingerprint'] == fingerprint:
            healthy(current)
            nginx_matches(config, current)
            return {'status': 'already_active', 'image': image, 'source_sha': source, 'version': version}
        if current:
            require(current['project'] == config['project'] and current['role'] == config['role'], 'current service ownership differs')
            healthy(current)
        release = directory / ('release-' + uuid.uuid4().hex)
        release.mkdir(mode=0o700)
        templates = (Path(__file__).resolve().parent / 'deploy/compose.yaml',
                     Path(__file__).resolve().parent.parent / 'deploy/compose.yaml')
        template = next((path for path in templates if path.is_file()), None)
        require(template is not None, 'deployment Compose template is missing')
        shutil.copyfile(template, release / 'compose.yaml')
        atomic_write(release / 'image.env', ('VIBEFACE_IMAGE=' + image + '\n').encode())
        write_json(release / 'runtime.json', compose_runtime(config, image, version, source, fingerprint))
        target = {'schema': 1, 'directory': str(release), 'project': config['project'], 'role': config['role'],
                  'image': image, 'tag': tag, 'source_sha': source, 'version': version, 'fingerprint': fingerprint}
        if config['role'] in CORE_ROLES:
            target['device_id'] = config['device_id']
        compose(target, 'config', '--quiet')
        journal = {'schema': 1, 'phase': 'prepared', 'config': config, 'target': target,
                   'previous': current, 'legacy_enabled': False}
        nginx_data = None
        if 'nginx_config' in config:
            nginx = checked_path(Path(config['nginx_config']))
            require(nginx.is_file() and nginx.stat().st_size <= 1024 * 1024, 'invalid nginx configuration file')
            command('nginx', '-t')
            old_nginx = nginx.read_bytes()
            nginx_data = transform_nginx(old_nginx.decode(), config).encode()
            target['nginx_sha256'] = hashlib.sha256(nginx_data).hexdigest()
            atomic_write(release / 'nginx.before', old_nginx)
            journal['nginx_backup'] = str(release / 'nginx.before')
        if config['role'] in CORE_ROLES:
            journal['identity_sha256'] = hashlib.sha256((Path(config['data_dir']) / 'identity.json').read_bytes()).hexdigest()
            if not current:
                command('systemctl', 'is-active', '--quiet', config['legacy_service'])
                journal['legacy_enabled'] = command('systemctl', 'is-enabled', config['legacy_service'], check=False).stdout.strip() == 'enabled'
        write_json(journal_path, journal)
        try:
            if config['role'] in CORE_ROLES:
                with lease(config, current):
                    journal['phase'] = 'stopping_previous'; write_json(journal_path, journal)
                    if current:
                        stop(current)
                    else:
                        command('systemctl', 'stop', config['legacy_service'])
                        require(command('systemctl', 'is-active', '--quiet', config['legacy_service'], check=False).returncode != 0,
                                'legacy service did not stop')
                with node_lock(config):
                    backup_data(config, release / 'data.before.tar.gz')
            elif current:
                journal['phase'] = 'stopping_previous'; write_json(journal_path, journal)
                stop(current)
            journal['phase'] = 'starting'; write_json(journal_path, journal)
            compose(target, 'up', '-d', '--no-build', '--pull', 'never', 'service')
            wait_healthy(target)
            if config['role'] in CORE_ROLES:
                require(hashlib.sha256((Path(config['data_dir']) / 'identity.json').read_bytes()).hexdigest()
                        == journal['identity_sha256'], 'new container changed the existing identity')
            if nginx_data is not None:
                journal['phase'] = 'switching_nginx'; write_json(journal_path, journal)
                nginx_apply(config, nginx_data, Path(config['nginx_config']).stat())
            if config['role'] in CORE_ROLES and not current and journal['legacy_enabled']:
                command('systemctl', 'disable', config['legacy_service'])
            healthy(target)
            nginx_matches(config, target)
            write_json(current_path, target)
            journal['phase'] = 'confirmed'; write_json(journal_path, journal)
        except BaseException:
            rollback(journal, journal_path)
            raise
        return {'status': 'activated', 'image': image, 'source_sha': source, 'version': version}
    finally:
        os.close(descriptor)


def self_test():
    import copy
    import tempfile
    from unittest.mock import patch
    config = {'project': 'official-entry', 'role': 'entry', 'deploy_dir': '/srv/service-control/entry',
              'listen': '127.0.0.1:4900', 'data_dir': '/var/lib/official-entry', 'device_id': 'a' * 64,
              'uid': 1001, 'gid': 1001, 'legacy_service': 'official-entry.service',
              'maintenance_socket': '/tmp/private-node/maintenance.sock'}
    validate_config(config)
    for field, value in (('data_dir', '/var/lib/../other'), ('uid', 0), ('listen', '0.0.0.0:80'),
                         ('device_id', 'wrong'), ('legacy_service', '--help')):
        bad = dict(config, **{field: value})
        try:
            validate_config(bad)
        except ValueError:
            pass
        else:
            raise AssertionError(field)
    runtime = compose_runtime(config, 'sha256:' + 'b' * 64, '1.0.0', 'c' * 40, 'd' * 64)['services']['service']
    assert runtime['network_mode'] == 'host' and runtime['user'] == '1001:1001'
    assert runtime['volumes'][0]['bind']['create_host_path'] is False
    assert runtime['environment']['VIBEFACE_ROLE'] == 'entry'
    nginx = '''server { listen 80; server_name app.example.invalid; return 301 https://$host$request_uri; }
server {
    listen 443 ssl;
    server_name app.example.invalid;
    root /srv/static-old;
    add_header Content-Security-Policy "old policy" always;
    add_header X-Vibeface-Source old;
    location = /api/client { proxy_pass http://127.0.0.1:4900; }
    location ^~ /bootstrap/ { proxy_pass http://127.0.0.1:4900; }
    location ~ ^/(relay|ping|generate_204)$ { proxy_pass http://127.0.0.1:3900; }
    location = /manifest.webmanifest { types {} default_type application/manifest+json; try_files $uri =404; }
    location ^~ /assets/ { try_files $uri =404; }
    location ~* \\.(?:js|wasm|css|woff2?|svg|png|ico|json|webmanifest)$ { try_files $uri =404; }
    location / { try_files $uri $uri/ /_shell.html; }
}
'''
    app = {'role': 'app', 'server_name': 'app.example.invalid', 'listen': '127.0.0.1:8088'}
    updated = transform_nginx(nginx, app)
    assert 'old policy' not in updated and 'X-Vibeface-Source old' not in updated and 'try_files' not in updated
    for node in nginx_nodes(nginx)[1]['children']:
        if node['words'][0] == 'location' and any(token in ' '.join(node['words']) for token in ('api/client', 'bootstrap', 'relay|')):
            assert nginx[node['start']:node['end']] in updated
    assert transform_nginx(updated, app) == updated
    relay_source = '''server { listen 443 ssl; server_name app.example.invalid; root /old;
    location ~ ^/(relay|ping|generate_204)$ { proxy_pass http://127.0.0.1:3340; }
    location / { proxy_pass http://127.0.0.1:4900; }
}'''
    relay = transform_nginx(relay_source, dict(app, role='relay'))
    assert 'location / {\n        return 404;' in relay and 'location ~ ^/(relay|ping|generate_204)$' in relay
    for selector in ('location = /index.html { try_files $uri =404; }',
                     'location ^~ /assets/ { alias /old/; }'):
        try:
            transform_nginx(relay_source.replace('root /old;', 'root /old; ' + selector), dict(app, role='relay'))
        except ValueError:
            pass
        else:
            raise AssertionError('Relay kept a frontend route')
    for selector in ('location = /favicon.ico { alias /old/icon.ico; }',
                     'location /old/ { include /old/static.conf; }'):
        try:
            transform_nginx(nginx.replace('location / {', selector + '\n    location / {', 1), app)
        except ValueError:
            pass
        else:
            raise AssertionError('unknown static route accepted')
    for bad in (nginx.replace('app.example.invalid', 'other.example.invalid'), nginx + '\nserver {'):
        try:
            transform_nginx(bad, app)
        except ValueError:
            pass
        else:
            raise AssertionError('invalid nginx configuration accepted')
    previous_manifest = {'project': 'owned', 'fingerprint': 'old-config', 'image': 'sha256:' + 'b' * 64}
    actual_container = {'Config': {'Labels': {'com.docker.compose.project': 'owned',
                        'com.docker.compose.service': 'service', 'dev.vibeface.deployment': 'new-config'}},
                        'Image': previous_manifest['image'], 'State': {'Running': True}}
    with patch(__name__ + '.containers', return_value=['c' * 64]), \
            patch(__name__ + '.docker', return_value=json.dumps([actual_container])):
        assert not restored_previous(previous_manifest), 'same image with changed config treated as restored'
        actual_container['Config']['Labels']['dev.vibeface.deployment'] = 'old-config'
    with patch(__name__ + '.containers', return_value=['c' * 64]), \
            patch(__name__ + '.docker', return_value=json.dumps([actual_container])), \
            patch(__name__ + '.wait_healthy') as waiting:
        assert restored_previous(previous_manifest)
        waiting.assert_called_once_with(previous_manifest)
    events = []
    artifact_root = Path(__file__).resolve().parent.parent / '.artifacts'
    artifact_root.mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(dir=artifact_root, prefix='activate-check-') as temporary:
        directory = Path(temporary)
        nginx_file = directory / 'nginx.conf'; nginx_file.write_text('confirmed')
        nginx_state = {'nginx_sha256': hashlib.sha256(b'confirmed').hexdigest()}
        nginx_matches({'nginx_config': str(nginx_file)}, nginx_state)
        nginx_file.write_text('drifted')
        try:
            nginx_matches({'nginx_config': str(nginx_file)}, nginx_state)
        except ValueError:
            pass
        else:
            raise AssertionError('nginx drift reported as already active')
        state = {'schema': 1, 'phase': 'starting', 'target': {'project': 'owned'}, 'config': {'role': 'app'},
                 'previous': {'project': 'previous'}}
        with patch(__name__ + '.restored_previous', return_value=False), \
                patch(__name__ + '.stop', side_effect=lambda target: events.append('stop-new')), \
                patch(__name__ + '.compose', side_effect=lambda *a: events.append('start-previous')), \
                patch(__name__ + '.wait_healthy', side_effect=lambda *a: events.append('check-previous')):
            rollback(copy.deepcopy(state), directory / 'activation.json')
        assert events == ['stop-new', 'start-previous', 'check-previous']
        assert read_json(directory / 'activation.json')['phase'] == 'rolled_back'
        assert read_json(directory / 'current.json') == state['previous']
        events.clear()
        with patch(__name__ + '.restored_previous', return_value=True), \
                patch(__name__ + '.stop', side_effect=lambda *a: events.append('unsafe-stop')), \
                patch(__name__ + '.compose', side_effect=lambda *a: events.append('unsafe-restart')):
            rollback(copy.deepcopy(state), directory / 'activation.json')
        assert not events and read_json(directory / 'activation.json')['phase'] == 'rolled_back'
        events.clear()
        with patch(__name__ + '.stop', side_effect=lambda target: events.append('unsafe-stop')):
            rollback(dict(state, phase='prepared'), directory / 'activation.json')
        assert not events
        events.clear()
        with patch(__name__ + '.restored_previous', return_value=False), \
                patch(__name__ + '.stop', side_effect=ValueError('new process still running')), \
                patch(__name__ + '.compose', side_effect=lambda *a: events.append('unsafe-start')):
            try:
                rollback(copy.deepcopy(state), directory / 'activation.json')
            except ValueError:
                pass
        assert not events
        data = directory / 'data'; data.mkdir(); (data / 'identity.json').write_text('keep')
        (data / 'escape').symlink_to(directory)
        try:
            backup_data({'data_dir': str(data)}, directory / 'backup.tar.gz')
        except ValueError:
            pass
        else:
            raise AssertionError('backup followed a symbolic link')
        assert (data / 'identity.json').read_text() == 'keep'
    print('deployment activation self-test passed')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--self-test', action='store_true')
    parser.add_argument('--config', help='Private JSON file, or - for standard input')
    parser.add_argument('--image', help='Already imported sha256 image ID')
    parser.add_argument('--tag')
    parser.add_argument('--source-sha')
    parser.add_argument('--version')
    args = parser.parse_args()
    if args.self_test:
        self_test(); return
    require(all((args.config, args.image, args.tag, args.source_sha, args.version)), 'all activation pins are required')
    require(re.fullmatch(IMAGE, args.image) and re.fullmatch(r'[0-9a-f]{40}', args.source_sha)
            and re.fullmatch(VERSION, args.version), 'invalid activation pins')
    raw = sys.stdin.buffer.read(65537) if args.config == '-' else checked_path(Path(args.config)).read_bytes()
    require(len(raw) <= 65536, 'deployment configuration is too large')
    config = validate_config(json.loads(raw))
    require(args.tag == ('www-v' if config['role'] == 'www' else 'v') + args.version, 'tag and version differ')
    print(json.dumps(activate(config, args.image, args.tag, args.source_sha, args.version)))


if __name__ == '__main__':
    try:
        main()
    except (ValueError, KeyError, TypeError, OSError, subprocess.SubprocessError) as error:
        sys.exit('activation failed: ' + str(error) if isinstance(error, ValueError) else 'activation failed: ' + type(error).__name__)
