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
from unittest import TestCase


CORE_ROLES = ('node',)
ROLES = (*CORE_ROLES, 'app', 'www')
IMAGE = r'sha256:[0-9a-f]{64}'
VERSION = r'(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)(?:-rc\.[1-9][0-9]*|-nightly\.[0-9]{8}\.[1-9][0-9]*)?'
DOCKER = ['docker', '--host', 'unix:///var/run/docker.sock']
HEADER_NAMES = {'content-security-policy', 'x-content-type-options', 'x-frame-options',
                'referrer-policy', 'cache-control', 'x-vibeface-source', 'x-vibeface-version'}
STATIC_REGEX = r'\.(?:js|wasm|css|woff2?|svg|png|ico|json|webmanifest)$'
# Mounted one by one over the Node's own copies, so a certificate client must rewrite them in place.
TLS_FILES = ('tls-cert.pem', 'tls-key.pem')
UNIT_DIRS = ('/etc/systemd/system', '/run/systemd/system', '/usr/local/lib/systemd/system',
             '/usr/lib/systemd/system', '/lib/systemd/system')


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
    core = config['role'] in CORE_ROLES
    required = common | ({'data_dir', 'tls_dir', 'device_id', 'uid', 'gid'} if core else {'nginx_config', 'server_name'})
    # A Node that replaces a host service names it and its maintenance socket; a fresh install names neither.
    legacy = {'legacy_service', 'maintenance_socket'} if core else set()
    require(required <= config.keys() and config.keys() <= required | legacy | {'public_url'}
            and (legacy <= config.keys() or not legacy & config.keys()),
            'unsupported or missing deployment configuration fields')
    require(re.fullmatch(r'[a-z][a-z0-9-]{0,47}', config['project']), 'invalid Compose project')
    # Browsers and devices reach the official node directly; a static container only its own host's nginx.
    host = '0.0.0.0' if config['role'] in CORE_ROLES else '127.0.0.1'
    require(re.fullmatch(re.escape(host) + r':[1-9][0-9]{0,4}', config['listen'])
            and int(config['listen'].split(':')[1]) <= 65535, 'listen must be an explicit port on ' + host)
    for name in ('deploy_dir', 'data_dir', 'tls_dir', 'nginx_config', 'maintenance_socket'):
        if name in config:
            canonical_path(config[name])
    if config['role'] in CORE_ROLES:
        require(re.fullmatch(r'[0-9a-f]{64}', config['device_id'])
                and all(type(config[n]) is int and 0 < config[n] <= 2**31 - 1 for n in ('uid', 'gid'))
                and ('legacy_service' not in config
                     or re.fullmatch(r'[a-zA-Z0-9][a-zA-Z0-9_.@-]{0,127}\.service', config['legacy_service'])),
                'invalid existing service identity')
        paths = [Path(config[name]) for name in ('data_dir', 'deploy_dir', 'tls_dir')]
        require(all(a != b and a not in b.parents for a in paths for b in paths if a is not b),
                'deployment files, Node data and TLS files must be separate')
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


def import_image(archive, digest):
    require(re.fullmatch(r'[0-9a-f]{64}', digest), 'invalid image archive digest')
    archive = checked_path(canonical_path(archive))
    require(archive.is_file(), 'image archive is not a regular file')
    checksum = hashlib.sha256()
    with archive.open('rb') as source:
        while block := source.read(1024 * 1024):
            checksum.update(block)
    require(checksum.hexdigest() == digest, 'transferred image archive digest differs')
    result = docker('load', '--input', str(archive), timeout=300)
    match = re.fullmatch(r'Loaded image ID: (' + IMAGE + r')', result)
    require(match is not None, 'Docker did not return one immutable imported image ID')
    return match[1]


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


def running(manifest):
    # Health reloads config.json, which a release may rewrite in its own format before activation;
    # the maintenance lease proves the running Core's identity instead.
    container, info = container_info(manifest)
    require(info['State']['Status'] == 'running', 'previous container is not running at the pinned image')
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
                                 'bind': {'create_host_path': False}},
                                *({'type': 'bind', 'source': config['tls_dir'] + '/' + name, 'target': '/data/' + name,
                                   'read_only': True, 'bind': {'create_host_path': False}} for name in TLS_FILES)],
                       # Each relay or browser connection takes descriptors; containers often start
                       # with a soft limit of 1024.
                       ulimits={'nofile': {'soft': 65536, 'hard': 65536}})
    else:
        service.update(user='101:101', ports=[{'target': 8080, 'published': config['listen'].split(':')[1],
                                             'host_ip': '127.0.0.1', 'protocol': 'tcp'}])
    return {'services': {'service': service}}


def tls_directory(config, owner=0):
    """The root directory the host's certificate client writes, which the Node may read but not change."""
    tls = checked_path(Path(config['tls_dir']))
    metadata = tls.lstat()
    require(stat.S_ISDIR(metadata.st_mode) and metadata.st_uid == owner and metadata.st_gid == config['gid']
            and stat.S_IMODE(metadata.st_mode) == 0o750, 'TLS directory must be root:gid with mode 0750')
    for name, group, mode in (('tls-cert.pem', None, 0o644), ('tls-key.pem', config['gid'], 0o640)):
        metadata = checked_path(tls / name).lstat()
        require(stat.S_ISREG(metadata.st_mode) and metadata.st_uid == owner and stat.S_IMODE(metadata.st_mode) == mode
                and group in (None, metadata.st_gid), 'TLS certificate must be root 0644 and its key root:gid 0640')
    return tls


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
        for name in ('identity.json', 'data-epoch.json', 'config.json', 'node.lock', 'tls-cert.pem', 'tls-key.pem'):
            checked_path(data / name)
            require((data / name).is_file(), 'initialized Node files are required')
        epoch = read_json(data / 'data-epoch.json')
        require(epoch == {'data_epoch': component['data_epoch']} and type(epoch['data_epoch']) is int,
                'data epoch differs; migration is not allowed')
        tls = tls_directory(config)
        options = ['--user', f'{config["uid"]}:{config["gid"]}', '--tmpfs', '/tmp:rw,nosuid,noexec,mode=1777',
                   '--mount', f'type=bind,src={data},dst=/data,readonly']
        for name in TLS_FILES:
            options.extend(('--mount', f'type=bind,src={tls / name},dst=/data/{name},readonly'))
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
    def indent(start):
        return text[text.rfind('\n', 0, start) + 1:start]
    def location(selector, body, start):
        return 'location ' + selector + ' {\n' + indent(start) + '    ' + body + '\n' + indent(start) + '}'
    edits, catchall = [], []
    for node in targets[0]['children']:
        words = node['words']
        if words[0] != 'location':
            require(words[0] != 'include', 'target nginx server must not include unverified routes')
            if words[0] in ('root', 'index') or (words[0] == 'add_header' and len(words) >= 3 and words[1].lower() in HEADER_NAMES):
                edits.append((node['start'], node['end'], ''))
            continue
        selector = ' '.join(words[1:])
        is_catchall = words == ['location', '/']
        if is_catchall:
            catchall.append(node['start'])
        is_static = is_catchall or (len(words) == 3 and words[1] in ('=', '^~') and words[2] in (
            '/assets/', '/generated/', '/manifest.webmanifest', '/vibeface_secure_bg.wasm',
            '/vibeface_secure.js', '/_shell.html', '/index.html', '/icon.svg', '/sw.js')) \
            or len(words) == 3 and words[1] in ('~', '~*') and words[2] == STATIC_REGEX
        directives = {n['words'][0] for n in node['children'] or []}
        require('try_files' not in directives or is_static, 'unrecognized static nginx location')
        if is_static:
            require(directives <= {'try_files', 'types', 'default_type', 'add_header', 'expires', 'access_log',
                                   'root', 'index', 'proxy_pass', 'proxy_set_header', 'proxy_http_version',
                                   'proxy_intercept_errors', 'proxy_cache'}, 'unsupported static nginx directives')
            body = ('proxy_pass http://' + config['listen'] + ';\n'
                    '        proxy_http_version 1.1;\n        proxy_set_header Host $host;\n'
                    '        proxy_set_header Connection "";\n        proxy_intercept_errors off;\n        proxy_cache off;')
        else:
            # Browsers call the official node directly, so the page host answers no API or relay route.
            require(bool(node['children']) and all(n['words'] == ['return', '404'] for n in node['children']),
                    'unrecognized static nginx location; API and relay routes must only return 404')
            continue
        edits.append((node['start'], node['end'], location(selector, body, node['start'])))
    require(len(catchall) == 1, 'expected nginx routes are missing')
    for start, end, replacement in sorted(edits, reverse=True):
        text = text[:start] + replacement + text[end:]
    nginx_nodes(text)
    return text


@contextmanager
def lease(config, previous):
    if previous:
        container = running(previous)
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
    elif 'legacy_service' in config:
        with legacy_channel(config, 'acquire') as health:
            yield health
    else:
        yield None


@contextmanager
def legacy_channel(config, request):
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
        stream.sendall((json.dumps({'protocol': 1, 'command': request}) + '\n').encode())
        packet = bytearray()
        while not packet.endswith(b'\n') and len(packet) <= 4096:
            part = stream.recv(4096)
            require(part, 'legacy maintenance connection ended early')
            packet.extend(part)
        value = json.loads(packet)
        require(len(packet) <= 4096 and value.get('protocol') == 1 and value.get('status') == ('ready' if request == 'acquire' else 'healthy')
                and value['health']['device_id'] == config['device_id'], 'legacy maintenance lease was refused')
        yield value['health']
    finally:
        stream.close()

def units_using(path, roots=UNIT_DIRS):
    """Unit files and drop-ins naming `path` or a path under it in any field, read as text for systemd 219."""
    pattern = re.compile(re.escape(path) + r'(?![\w.-])')
    found = set()
    for root in roots:
        for directory, _, names in os.walk(root):
            for name in names:
                unit = Path(directory) / name
                try:
                    if unit.is_file() and unit.stat().st_size <= 1024 * 1024 \
                            and pattern.search(unit.read_text(errors='replace')):
                        found.add(str(unit))
                except OSError:
                    continue
    return sorted(found)


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


def previous_stopped(previous):
    if not previous:
        return False
    try:
        _, info = container_info(previous)
    except (ValueError, KeyError, json.JSONDecodeError):
        return False
    return not info['State']['Running'] and info['State']['Pid'] == 0


def rollback(journal, journal_path):
    target, config = journal['target'], journal['config']
    phase = journal['phase']
    if phase == 'prepared':
        journal['phase'] = 'rolled_back'
        write_json(journal_path, journal)
        return
    previous = journal['previous']
    restored = restored_previous(previous)
    legacy_restored = False
    if not previous and 'legacy_service' in config:
        if command('systemctl', 'is-active', '--quiet', config['legacy_service'], check=False).returncode == 0:
            with legacy_channel(config, 'probe') as health:
                require(health['version'] == journal['legacy_version'], 'legacy running version differs')
            stopped(target)
            legacy_restored = True
    if phase != 'stopping_previous' and not restored and not legacy_restored and not previous_stopped(previous):
        stop(target)
    if phase == 'switching_nginx' and journal.get('nginx_backup'):
        path = checked_path(Path(config['nginx_config']))
        nginx_apply(config, Path(journal['nginx_backup']).read_bytes(), path.stat())
    if config['role'] in CORE_ROLES and not restored and not legacy_restored:
        with node_lock(config):
            require(hashlib.sha256((Path(config['data_dir']) / 'identity.json').read_bytes()).hexdigest()
                    == journal['identity_sha256'], 'identity changed; automatic rollback was refused')
        if previous:
            compose(previous, 'up', '-d', '--no-build', '--pull', 'never', 'service')
            wait_healthy(previous)
        elif 'legacy_service' in config:
            command('systemctl', 'start', config['legacy_service'])
            if journal['legacy_enabled']:
                command('systemctl', 'enable', config['legacy_service'])
            command('systemctl', 'is-active', '--quiet', config['legacy_service'])
    elif previous and not restored:
        compose(previous, 'up', '-d', '--no-build', '--pull', 'never', 'service')
        wait_healthy(previous)
    if legacy_restored and journal['legacy_enabled']:
        command('systemctl', 'enable', config['legacy_service'])
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
            if config['role'] in CORE_ROLES:
                running(current)
            else:
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
            if not current and 'legacy_service' in config:
                command('systemctl', 'is-active', '--quiet', config['legacy_service'])
                journal['legacy_enabled'] = command('systemctl', 'is-enabled', config['legacy_service'], check=False).stdout.strip() == 'enabled'
            elif not current:
                # A fresh install: no service of this project may already run on this data, and no host
                # service may own it either; one that does is replaced only as `legacy_service`.
                units = units_using(config['data_dir'])
                require(not units, 'systemd units use the data directory; configure legacy_service to replace '
                        + ', '.join(units))
                stopped(target)
        write_json(journal_path, journal)
        try:
            if config['role'] in CORE_ROLES:
                with lease(config, current) as health:
                    if health:
                        journal['legacy_version'] = health['version']
                    if current or health:
                        journal['phase'] = 'stopping_previous'; write_json(journal_path, journal)
                    if current:
                        stop(current)
                    elif health:
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
        prune(directory, release, current, image)
        return {'status': 'activated', 'image': image, 'source_sha': source, 'version': version}
    finally:
        os.close(descriptor)


def prune(directory, release, previous, image):
    # The backups and the old release only serve a rollback; once confirmed, the host keeps the new one.
    for name in ('nginx.before', 'data.before.tar.gz'):
        (release / name).unlink(missing_ok=True)
    for old in directory.glob('release-*'):
        if old != release:
            shutil.rmtree(old)
    if previous and previous['image'] != image:
        docker('image', 'rm', previous['image'], check=False)


def self_test():
    import copy
    import tempfile
    from unittest.mock import patch
    with tempfile.TemporaryDirectory() as temporary:
        archive = Path(temporary).resolve() / 'image.tar.gz'
        archive.write_bytes(b'verified archive')
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        imported = 'sha256:' + 'c' * 64
        with patch(__name__ + '.docker', return_value='Loaded image ID: ' + imported) as load:
            assert import_image(str(archive), digest) == imported
            load.assert_called_once_with('load', '--input', str(archive), timeout=300)
        with patch(__name__ + '.docker') as load:
            with TestCase().assertRaises(ValueError, msg='imported an unverified archive'):
                import_image(str(archive), '0' * 64)
            load.assert_not_called()
        for result in ('Loaded image: moving:tag', 'Loaded image ID: ' + imported + '\nLoaded image ID: ' + imported):
            with patch(__name__ + '.docker', return_value=result):
                with TestCase().assertRaises(ValueError, msg='accepted an ambiguous imported image'):
                    import_image(str(archive), digest)
    config = {'project': 'official-node', 'role': 'node', 'deploy_dir': '/srv/service-control/node',
              'listen': '0.0.0.0:4792', 'data_dir': '/var/lib/official-node', 'tls_dir': '/etc/official-node/tls',
              'device_id': 'a' * 64,
              'uid': 1001, 'gid': 1001, 'legacy_service': 'official-node.service',
              'maintenance_socket': '/tmp/private-node/maintenance.sock'}
    app = {'project': 'official-app', 'role': 'app', 'deploy_dir': '/srv/service-control/app',
           'listen': '127.0.0.1:8088', 'nginx_config': '/etc/nginx/conf.d/app.conf',
           'server_name': 'app.example.invalid'}
    fresh = {key: item for key, item in config.items() if key not in ('legacy_service', 'maintenance_socket')}
    validate_config(config); validate_config(app); validate_config(fresh)
    for base, field, value in ((config, 'data_dir', '/var/lib/../other'), (config, 'uid', 0),
                               (config, 'tls_dir', None), (config, 'tls_dir', '/var/lib/official-node/tls'),
                               (config, 'tls_dir', '/srv/service-control/node'),
                               (config, 'listen', '127.0.0.1:4792'), (config, 'device_id', 'wrong'),
                               (config, 'legacy_service', '--help'), (config, 'legacy_service', None),
                               (config, 'maintenance_socket', None),
                               (config, 'node_url', 'https://203.0.113.7:4792'),
                               (app, 'listen', '0.0.0.0:8088'), (app, 'node_url', 'https://203.0.113.7:4792'),
                               (app, 'legacy_service', 'official-node.service')):
        bad = {key: item for key, item in dict(base, **{field: value}).items() if item is not None}
        with TestCase().assertRaises(ValueError, msg=field):
            validate_config(bad)
    runtime = compose_runtime(config, 'sha256:' + 'b' * 64, '1.0.0', 'c' * 40, 'd' * 64)['services']['service']
    assert runtime['network_mode'] == 'host' and runtime['user'] == '1001:1001'
    assert runtime['volumes'][0]['bind']['create_host_path'] is False
    assert runtime['environment']['VIBEFACE_ROLE'] == 'node'
    assert runtime['ulimits'] == {'nofile': {'soft': 65536, 'hard': 65536}}
    assert [(v['source'], v['target'], v.get('read_only')) for v in runtime['volumes'][1:]] == [
        ('/etc/official-node/tls/tls-cert.pem', '/data/tls-cert.pem', True),
        ('/etc/official-node/tls/tls-key.pem', '/data/tls-key.pem', True)]
    nginx = '''server { listen 80; server_name app.example.invalid; return 301 https://$host$request_uri; }
server {
    listen 443 ssl;
    server_name app.example.invalid;
    root /srv/static-old;
    add_header Content-Security-Policy "old policy" always;
    add_header X-Vibeface-Source old;
    location = /api/client { return 404; }
    location = /api/info { return 404; }
    location = /sdk.v1.js { return 404; }
    location = /manifest.webmanifest { types {} default_type application/manifest+json; try_files $uri =404; }
    location ^~ /assets/ { try_files $uri =404; }
    location ~* \\.(?:js|wasm|css|woff2?|svg|png|ico|json|webmanifest)$ { try_files $uri =404; }
    location / { try_files $uri $uri/ /_shell.html; }
}
'''
    updated = transform_nginx(nginx, app)
    assert 'old policy' not in updated and 'X-Vibeface-Source old' not in updated and 'try_files' not in updated
    assert 'location = /sdk.v1.js { return 404; }' in updated and 'location = /api/client { return 404; }' in updated
    assert 'proxy_pass https://' not in updated and '/api/decision' not in updated
    assert transform_nginx(updated, app) == updated == transform_nginx(nginx, dict(app, role='www'))
    for selector in ('location = /favicon.ico { alias /old/icon.ico; }',
                     'location /old/ { include /old/static.conf; }', 'location /old/ {}',
                     'location = /api/decision { proxy_pass https://203.0.113.7:4792; }',
                     'location ^~ /bootstrap/ { proxy_pass http://127.0.0.1:4900; }',
                     'location ~ ^/(relay|ping|generate_204)$ { proxy_pass http://127.0.0.1:3340; }'):
        with TestCase().assertRaises(ValueError, msg='unknown static route accepted'):
            transform_nginx(nginx.replace('location / {', selector + '\n    location / {', 1), app)
    for bad in (nginx.replace('app.example.invalid', 'other.example.invalid'), nginx + '\nserver {',
                nginx.replace('location / {', 'include /old/routes.conf; location / {', 1),
                nginx.replace('location = /api/client { return 404; }',
                              'location = /api/client { proxy_pass http://127.0.0.1:4900; }', 1)):
        with TestCase().assertRaises(ValueError, msg='invalid nginx configuration accepted'):
            transform_nginx(bad, app)
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
        with TestCase().assertRaises(ValueError, msg='nginx drift reported as already active'):
            nginx_matches({'nginx_config': str(nginx_file)}, nginx_state)
        state = {'schema': 1, 'phase': 'starting', 'target': {'project': 'owned'}, 'config': {'role': 'app'},
                 'previous': {'project': 'previous'}}
        with patch(__name__ + '.restored_previous', return_value=False), \
                patch(__name__ + '.previous_stopped', return_value=False), \
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
        with patch(__name__ + '.restored_previous', return_value=False), \
                patch(__name__ + '.previous_stopped', return_value=True), \
                patch(__name__ + '.stop', side_effect=lambda *a: events.append('unsafe-stop')), \
                patch(__name__ + '.compose', side_effect=lambda *a: events.append('start-previous')), \
                patch(__name__ + '.wait_healthy', side_effect=lambda *a: events.append('check-previous')):
            rollback(copy.deepcopy(state), directory / 'activation.json')
        assert events == ['start-previous', 'check-previous']
        events.clear()
        @contextmanager
        def restored_legacy(*args):
            yield {'version': '1.0.0'}
        legacy_state = dict(state, previous=None, legacy_version='1.0.0', legacy_enabled=False,
                            config={'role': 'node', 'legacy_service': 'fixture.service'})
        with patch(__name__ + '.legacy_channel', side_effect=restored_legacy), \
                patch(__name__ + '.command', return_value=subprocess.CompletedProcess([], 0, '', '')), \
                patch(__name__ + '.stopped', side_effect=lambda *a: events.append('verify-target-stopped')), \
                patch(__name__ + '.stop', side_effect=lambda *a: events.append('unsafe-stop')):
            rollback(legacy_state, directory / 'activation.json')
        assert events == ['verify-target-stopped']
        events.clear()
        with patch(__name__ + '.stop', side_effect=lambda target: events.append('unsafe-stop')):
            rollback(dict(state, phase='prepared'), directory / 'activation.json')
        assert not events
        events.clear()
        with lease(fresh, None) as health:
            assert health is None
        fresh_data = directory / 'fresh'; fresh_data.mkdir()
        (fresh_data / 'identity.json').write_text('fresh'); (fresh_data / 'node.lock').touch()
        fresh_state = dict(state, previous=None, legacy_enabled=False, config=dict(fresh, data_dir=str(fresh_data)),
                           identity_sha256=hashlib.sha256(b'fresh').hexdigest())
        with patch(__name__ + '.stop', side_effect=lambda target: events.append('stop-new')), \
                patch(__name__ + '.command', side_effect=lambda *a, **k: events.append('unsafe-systemctl')), \
                patch(__name__ + '.compose', side_effect=lambda *a: events.append('unsafe-start')):
            rollback(fresh_state, directory / 'activation.json')
        assert events == ['stop-new'] and not (directory / 'current.json').exists()
        assert read_json(directory / 'activation.json')['phase'] == 'rolled_back'
        events.clear()
        with patch(__name__ + '.restored_previous', return_value=False), \
                patch(__name__ + '.previous_stopped', return_value=False), \
                patch(__name__ + '.stop', side_effect=ValueError('new process still running')), \
                patch(__name__ + '.compose', side_effect=lambda *a: events.append('unsafe-start')):
            try:
                rollback(copy.deepcopy(state), directory / 'activation.json')
            except ValueError:
                pass
        assert not events
        tls = directory / 'tls'; tls.mkdir(); tls.chmod(0o750)
        (tls / 'tls-cert.pem').write_text('cert'); (tls / 'tls-cert.pem').chmod(0o644)
        (tls / 'tls-key.pem').write_text('key'); (tls / 'tls-key.pem').chmod(0o640)
        owned = dict(config, tls_dir=str(tls), gid=os.getegid())
        assert tls_directory(owned, owner=os.geteuid()) == tls
        for unsafe in (lambda: (tls / 'tls-key.pem').chmod(0o660), lambda: (tls / 'tls-key.pem').chmod(0o644),
                       lambda: tls.chmod(0o770), lambda: owned.update(gid=os.getegid() + 1)):
            unsafe()
            with TestCase().assertRaises(ValueError, msg='TLS files the Node could change or not read accepted'):
                tls_directory(owned, owner=os.geteuid())
            tls.chmod(0o750); (tls / 'tls-key.pem').chmod(0o640); owned['gid'] = os.getegid()
        with TestCase().assertRaises(ValueError, msg='TLS files not owned by root accepted'):
            tls_directory(owned)
        units = directory / 'units'; (units / 'old.service.d').mkdir(parents=True)
        (units / 'other.service').write_text('[Service]\nExecStart=/usr/bin/node --data-dir /var/lib/official-node2\n')
        assert units_using('/var/lib/official-node', [str(units)]) == []
        (units / 'old.service.d' / 'data.conf').write_text('[Service]\nWorkingDirectory=/var/lib/official-node\n')
        (units / 'old.service').write_text('[Service]\nExecStart=/usr/bin/node --data-dir=/var/lib/official-node/\n')
        assert units_using('/var/lib/official-node', [str(units)]) == [
            str(units / 'old.service'), str(units / 'old.service.d' / 'data.conf')]
        data = directory / 'data'; data.mkdir(); (data / 'identity.json').write_text('keep')
        (data / 'escape').symlink_to(directory)
        with TestCase().assertRaises(ValueError, msg='backup followed a symbolic link'):
            backup_data({'data_dir': str(data)}, directory / 'backup.tar.gz')
        assert (data / 'identity.json').read_text() == 'keep'
    with tempfile.TemporaryDirectory() as temporary:
        directory = Path(temporary)
        old, new = directory / 'release-old', directory / 'release-new'
        for release in (old, new):
            release.mkdir()
            for name in ('compose.yaml', 'nginx.before', 'data.before.tar.gz'):
                (release / name).write_text(name)
        with patch(__name__ + '.docker') as remove:
            prune(directory, new, {'image': 'sha256:old'}, 'sha256:new')
        assert sorted(path.name for path in directory.iterdir()) == ['release-new']
        assert [path.name for path in new.iterdir()] == ['compose.yaml'], 'confirmed release keeps no backups'
        remove.assert_called_once_with('image', 'rm', 'sha256:old', check=False)
    print('deployment activation self-test passed')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--self-test', action='store_true')
    parser.add_argument('--config', help='Private JSON file, or - for standard input')
    parser.add_argument('--image', help='Already imported sha256 image ID')
    parser.add_argument('--archive', help='Import this verified image archive before activation')
    parser.add_argument('--archive-sha256')
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
    require(bool(args.archive) == bool(args.archive_sha256), 'archive and its digest must be supplied together')
    if args.archive:
        args.image = import_image(args.archive, args.archive_sha256)
    print(json.dumps(activate(config, args.image, args.tag, args.source_sha, args.version)))


if __name__ == '__main__':
    try:
        main()
    except (ValueError, KeyError, TypeError, OSError, subprocess.SubprocessError) as error:
        sys.exit('activation failed: ' + str(error) if isinstance(error, ValueError) else 'activation failed: ' + type(error).__name__)
