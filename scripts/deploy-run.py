#!/usr/bin/env python3
"""Build verified deployment images and activate them through a pinned SSH host."""
import argparse
import gzip
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
import uuid

import deploy
import release

ROOT = Path(__file__).resolve().parent.parent


def checked(args, **kwargs):
    return subprocess.run(args, check=True, **kwargs)


def image_id(image):
    value = subprocess.check_output(['docker', 'image', 'inspect', '--format', '{{.Id}}', image], text=True).strip()
    release.require(re.fullmatch(r'sha256:[0-9a-f]{64}', value), 'invalid Docker image identity')
    return value


def build(service, tag, output):
    output = Path(output).resolve()
    info = deploy.prepare(service, tag, output)
    images = {}
    for component in ('core', 'app', 'www'):
        context = output / component
        if not context.exists():
            continue
        shutil.copy2(ROOT / 'deploy/runtime.py', context / 'runtime.py')
        if component == 'core':
            shutil.copy2(ROOT / 'config/public-bootstrap.json', context / 'public-bootstrap.json')
            dockerfile = ROOT / 'deploy/core.Dockerfile'
        else:
            checked([sys.executable, '-B', str(ROOT / 'deploy/static-config.py'),
                     '--role', component, '--component', str(context / 'component'),
                     '--output', str(context / 'nginx.conf')])
            dockerfile = ROOT / 'deploy/static.Dockerfile'
        name = f"vibeface-{component}:{info['source_sha']}"
        arguments = ['--build-arg', 'STATIC_ROOT=site'] if component == 'www' else []
        checked(['docker', 'build', '--platform', 'linux/amd64', *arguments, '--file', str(dockerfile),
                 '--label', f"org.opencontainers.image.revision={info['source_sha']}",
                 '--label', f"org.opencontainers.image.version={info['version']}",
                 '--tag', name, str(context)])
        identity = image_id(name)
        images[component] = identity
        if component == 'core':
            version = subprocess.check_output(['docker', 'run', '--rm', '--platform', 'linux/amd64',
                                              '--entrypoint', '/opt/vibeface/vibeface-core', identity,
                                              '--version'], text=True).strip()
            release.require(version == 'vibeface-core ' + info['version'], 'container Core version mismatch')
            refused = subprocess.run(['docker', 'run', '--rm', '--platform', 'linux/amd64', identity],
                                     capture_output=True, timeout=60)
            release.require(refused.returncode != 0, 'Core container initialized missing deployment data')
            core_smoke(identity, info, output)
        else:
            checked(['docker', 'run', '--rm', '--platform', 'linux/amd64', '--entrypoint', 'nginx', identity, '-t'])
            static_smoke(identity, context / 'component', component, info['source_sha'])
        archive = output / f'{component}.image.tar.gz'
        with gzip.open(archive, 'wb') as compressed:
            process = subprocess.Popen(['docker', 'save', identity], stdout=subprocess.PIPE)
            shutil.copyfileobj(process.stdout, compressed)
            process.stdout.close()
            release.require(process.wait() == 0, 'cannot export container image')
    info['images'] = images
    (output / 'deployment.json').write_text(json.dumps(info, indent=2) + '\n')
    return info


def core_smoke(identity, info, output):
    volume = 'vibeface-deploy-check-' + uuid.uuid4().hex
    def docker(*args, **kwargs):
        return subprocess.check_output(['docker', *args], text=True, **kwargs)
    tls = volume + '-tls'
    docker('volume', 'create', volume)
    docker('volume', 'create', tls)
    container = None
    base = ['--platform', 'linux/amd64', '--network', 'none', '-v', volume + ':/data']
    try:
        # The official node's shape, offline; the runtime guard refuses anything less bounded.
        gateway = 'https://127.0.0.1:4793'
        options = {'data_dir': '/data', 'listen': '127.0.0.1:4793', 'public_url': gateway, 'tls': True,
                   'allowed_origins': ['https://app.vibeface.dev'], 'relay_listen': '127.0.0.1:4792',
                   'relay_url': 'http://127.0.0.1:4792', 'relay_open': True, 'relay_rate': 16384,
                   'relay_max_clients': 128, 'relay_max_clients_per_ip': 8, 'forward_any': True,
                   'forward_sessions': 32, 'forward_queue': 192, 'forward_rate': 262144, 'forward_per_ip': 8,
                   'mesh_network': 'deploy-fixture'}
        result = json.loads(docker('run', '--rm', '-i', *base, '--entrypoint',
                                  '/opt/vibeface/vibeface-core', identity, '--control',
                                  input=json.dumps({'command': 'init', 'options': options})))
        release.require(result.get('ok') is True, 'isolated Core fixture initialization failed')
        # Like the host's root-owned TLS directory: root writes the pair, the Node only reads it.
        docker('run', '--rm', *base, '-v', tls + ':/tls', '--entrypoint', 'python3', identity, '-c',
               "from pathlib import Path; import os, shutil; p=Path('/data'); os.chmod(p,0o700); (p/'node.lock').touch(); "
               "[os.chown(x,1001,1001) for x in [p,*p.rglob('*')]]; "
               "[(shutil.copyfile(p/n,'/tls/'+n), os.chown('/tls/'+n,0,1001), os.chmod('/tls/'+n,m)) "
               "for n,m in (('tls-cert.pem',0o644),('tls-key.pem',0o640))]")
        for name in ('tls-cert.pem', 'tls-key.pem'):
            base += ['--mount', f'type=volume,src={tls},dst=/data/{name},volume-subpath={name},readonly']
        result = json.loads(docker('run', '--rm', '-i', *base, '--user', '1001:1001', '--entrypoint',
                                  '/opt/vibeface/vibeface-core', identity, '--control',
                                  input='{"command":"inspect","data_dir":"/data"}'))
        device = result['result']['device_id']
        environment = ['-e', 'VIBEFACE_ROLE=node', '-e', 'VIBEFACE_LISTEN=127.0.0.1:4793',
                       '-e', 'VIBEFACE_EXPECTED_VERSION=' + info['version'],
                       '-e', 'VIBEFACE_EXPECTED_SOURCE=' + info['source_sha'],
                       '-e', 'VIBEFACE_EXPECTED_DEVICE=' + device]
        # The image pins the official issuer, so the fixture Node runs only under its own anchor.
        refused = subprocess.run(['docker', 'run', '--rm', *base, '--user', '1001:1001', *environment,
                                  identity, 'guard'], capture_output=True, timeout=60)
        release.require(refused.returncode != 0, 'the guard accepted a Node other than the pinned issuer')
        anchor = output / 'fixture-bootstrap.json'
        anchor.write_text(json.dumps({'issuer': device, 'udp_host': '127.0.0.1', 'udp_port': 4793,
                                      'gateway_url': gateway}))
        base += ['-v', f'{anchor}:/opt/vibeface/public-bootstrap.json:ro']
        container = docker('run', '-d', *base, '--init', '--read-only', '--user', '1001:1001',
                           '--cap-drop', 'ALL', '--security-opt', 'no-new-privileges:true',
                           '--tmpfs', '/tmp:rw,nosuid,noexec,size=64m,mode=1777', *environment, identity).strip()
        for _ in range(30):
            health = subprocess.run(['docker', 'exec', container, 'python3', '/opt/vibeface/runtime.py', 'health'],
                                    capture_output=True, text=True)
            if health.returncode == 0:
                break
            time.sleep(1)
        release.require(health.returncode == 0 and json.loads(health.stdout)['device_id'] == device,
                        'Core fixture health or identity mismatch')
        docker('stop', '--time', '30', container)
        state = json.loads(docker('inspect', container))[0]['State']
        release.require(state['ExitCode'] == 0 and not state['OOMKilled'], 'Core did not stop gracefully')
        docker('start', container)
        for _ in range(30):
            health = subprocess.run(['docker', 'exec', container, 'python3', '/opt/vibeface/runtime.py', 'health'],
                                    capture_output=True, text=True)
            if health.returncode == 0:
                break
            time.sleep(1)
        release.require(health.returncode == 0 and json.loads(health.stdout)['device_id'] == device,
                        'Core identity changed after restart')
    finally:
        if container:
            docker('rm', '-f', container)
        docker('volume', 'rm', volume, tls)


def static_smoke(identity, component, role, sha):
    container = subprocess.check_output(['docker', 'run', '-d', '--platform', 'linux/amd64', '--init', '--read-only',
                                         '--cap-drop', 'ALL', '--security-opt', 'no-new-privileges:true',
                                         '--tmpfs', '/tmp:rw,nosuid,noexec,size=64m,mode=1777', '-p', '127.0.0.1::8080', identity], text=True).strip()
    try:
        binding = subprocess.check_output(['docker', 'port', container, '8080/tcp'], text=True).strip()
        release.require(re.fullmatch(r'127\.0\.0\.1:[0-9]+', binding), 'invalid smoke binding')
        origin = 'http://' + binding
        directory = component / ('web' if role == 'app' else 'site')
        for _ in range(30):
            try:
                with urllib.request.urlopen(origin + '/', timeout=2) as response:
                    html = response.read()
                    release.require(response.headers.get('X-Vibeface-Source') == sha, 'static image source mismatch')
                break
            except (OSError, urllib.error.URLError):
                time.sleep(1)
        else:
            raise ValueError('static image did not become ready')
        expected = directory / ('_shell.html' if role == 'app' else 'index.html')
        release.require(html == expected.read_bytes(), 'static image entry differs from released bytes')
        if role == 'app':
            with urllib.request.urlopen(origin + '/connect', timeout=5) as response:
                release.require(response.read() == expected.read_bytes(), 'App SPA fallback changed')
        for path in ('/api/missing', '/assets/not-a-release-asset.js'):
            try:
                urllib.request.urlopen(origin + path, timeout=5)
            except urllib.error.HTTPError as error:
                release.require(error.code == 404, 'unexpected static error response')
            else:
                raise ValueError('static image serves a missing API or asset')
    finally:
        checked(['docker', 'rm', '-f', container], stdout=subprocess.DEVNULL)


def transport(environment):
    host, user = environment['DEPLOY_HOST'], environment['DEPLOY_USER']
    port = environment.get('DEPLOY_PORT') or '22'
    release.require(re.fullmatch(r'[a-zA-Z0-9][a-zA-Z0-9.-]*', host)
                    and re.fullmatch(r'[a-z_][a-z0-9_-]*', user)
                    and port.isdecimal() and 1 <= int(port) <= 65535, 'invalid SSH destination')
    for key in ('DEPLOY_SSH_KEY', 'DEPLOY_KNOWN_HOSTS', 'DEPLOY_CONFIG'):
        release.require(environment.get(key), 'missing deployment configuration: ' + key)
    return host, user, port


def activate(service, output):
    output = Path(output).resolve()
    info = json.loads((output / 'deployment.json').read_text())
    release.require(service in info['targets'], 'image was not prepared for this service')
    component = 'www' if service == 'www' else 'app' if service == 'app' else 'core'
    identity = info['images'][component]
    release.require(re.fullmatch(r'sha256:[0-9a-f]{64}', identity), 'invalid prepared image ID')
    host, user, port = transport(os.environ)
    config = json.loads(os.environ['DEPLOY_CONFIG'])
    release.require(config['role'] == service, 'environment role does not match selected service')
    with tempfile.TemporaryDirectory(dir=output, prefix='ssh-') as temporary:
        temporary = Path(temporary)
        os.chmod(temporary, 0o700)
        key = temporary / 'key'; known = temporary / 'known_hosts'; settings = temporary / 'config.json'
        for path, content in ((key, os.environ['DEPLOY_SSH_KEY']),
                              (known, os.environ['DEPLOY_KNOWN_HOSTS']),
                              (settings, json.dumps(config))):
            path.write_text(content + '\n'); os.chmod(path, 0o600)
        host_key = host if port == '22' else f'[{host}]:{port}'
        checked(['ssh-keygen', '-F', host_key, '-f', str(known)], stdout=subprocess.DEVNULL)
        options = ['-i', str(key), '-o', 'BatchMode=yes', '-o', 'StrictHostKeyChecking=yes',
                   '-o', f'UserKnownHostsFile={known}', '-o', 'ConnectTimeout=15',
                   '-o', 'ServerAliveInterval=15', '-o', 'ServerAliveCountMax=3']
        remote = f'{user}@{host}'
        ssh = ['ssh', *options, '-p', port, remote]
        stage = subprocess.check_output([*ssh, 'umask 077; mktemp -d /tmp/vibeface-deploy.XXXXXXXX'], text=True).strip()
        release.require(re.fullmatch(r'/tmp/vibeface-deploy\.[a-zA-Z0-9]+', stage), 'invalid remote staging directory')
        succeeded = False
        try:
            checked(['scp', *options, '-P', port, str(settings), str(ROOT / 'scripts/deploy-activate.py'),
                     str(output / f'{component}.image.tar.gz'), f'{remote}:{stage}/'], stdout=subprocess.DEVNULL)
            checked(['scp', *options, '-P', port, '-r', str(ROOT / 'deploy'), f'{remote}:{stage}/'],
                    stdout=subprocess.DEVNULL)
            archive_digest = release.digest(output / f'{component}.image.tar.gz')
            command = ['python3', stage + '/deploy-activate.py', '--config', stage + '/config.json',
                       '--image', identity, '--tag', info['tag'], '--source-sha', info['source_sha'],
                       '--version', info['version'], '--archive', stage + f'/{component}.image.tar.gz',
                       '--archive-sha256', archive_digest]
            # A dropped SSH connection leaves activation state for reconciliation, never an automatic retry.
            shell = ('set -eu; if [ "$(id -u)" != 0 ]; then sudo -n true; fi; ' +
                     ('exec ' if user == 'root' else 'exec sudo -n ') + shlex.join(command))
            receipt = json.loads(subprocess.check_output([*ssh, shell], text=True))
            release.require(receipt.get('status') in ('activated', 'already_active')
                            and re.fullmatch(r'sha256:[0-9a-f]{64}', receipt.get('image', ''))
                            and receipt.get('source_sha') == info['source_sha']
                            and receipt.get('version') == info['version'], 'invalid remote activation receipt')
            succeeded = True
        finally:
            if succeeded:
                subprocess.run([*ssh, 'rm -rf -- ' + shlex.quote(stage)],
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    return {'service': service, 'tag': info['tag'], 'source_sha': info['source_sha'],
            'image': receipt['image'], 'prepared_image': identity}


def rehearse(output):
    """Activate the prepared Core on this runner as a fixture host: fresh install, handover, then a no-op."""
    release.require(os.geteuid() == 0, 'the activation rehearsal runs as root on a disposable runner')
    output = Path(output).resolve()
    info = json.loads((output / 'deployment.json').read_text())
    identity = info['images']['core']
    host = Path(tempfile.mkdtemp(prefix='vibeface-rehearsal-'))
    data, tls = host / 'state', host / 'tls'
    public_url = 'https://127.0.0.1:4792'
    options = {'data_dir': '/data', 'listen': '0.0.0.0:4792', 'public_url': public_url, 'tls': True,
               'allowed_origins': ['https://app.vibeface.dev'], 'relay_listen': '0.0.0.0:3340',
               'relay_url': 'http://127.0.0.1:3340', 'relay_open': True, 'relay_rate': 16384,
               'relay_max_clients': 128, 'relay_max_clients_per_ip': 8, 'forward_any': True,
               'forward_sessions': 32, 'forward_queue': 192, 'forward_rate': 262144, 'forward_per_ip': 8,
               'mesh_network': 'deploy-fixture'}
    # The host initialization in deploy/README.md, step by step.
    data.mkdir()
    result = json.loads(subprocess.check_output(
        ['docker', 'run', '--rm', '-i', '--network', 'none', '-v', f'{data}:/data', '--entrypoint',
         '/opt/vibeface/vibeface-core', identity, '--control'],
        input=json.dumps({'command': 'init', 'options': options}), text=True))
    release.require(result.get('ok') is True, 'fixture Node initialization failed')
    (data / 'node.lock').touch()
    checked(['chown', '-R', '1001:1001', str(data)])
    os.chmod(data, 0o700)
    checked(['install', '-d', '-o', 'root', '-g', '1001', '-m', '0750', str(tls)])
    checked(['install', '-o', 'root', '-g', 'root', '-m', '0644', str(data / 'tls-cert.pem'), str(tls / 'tls-cert.pem')])
    checked(['install', '-o', 'root', '-g', '1001', '-m', '0640', str(data / 'tls-key.pem'), str(tls / 'tls-key.pem')])
    device = json.loads(subprocess.check_output(
        ['docker', 'run', '--rm', '-i', '--network', 'none', '-v', f'{data}:/data', '--user', '1001:1001',
         '--entrypoint', '/opt/vibeface/vibeface-core', identity, '--control'],
        input='{"command":"inspect","data_dir":"/data"}', text=True))['result']['device_id']
    # The image pins the official issuer, so the rehearsal pins this fixture Node instead.
    anchor = host / 'public-bootstrap.json'
    anchor.write_text(json.dumps({'issuer': device, 'udp_host': '127.0.0.1', 'udp_port': 4792, 'gateway_url': public_url}))
    container = subprocess.check_output(['docker', 'create', identity], text=True).strip()
    try:
        checked(['docker', 'cp', str(anchor), container + ':/opt/vibeface/public-bootstrap.json'])
        fixture = subprocess.check_output(['docker', 'commit', container], text=True).strip()
    finally:
        checked(['docker', 'rm', container], stdout=subprocess.DEVNULL)
    config = {'project': 'vibeface-rehearsal', 'role': 'node', 'deploy_dir': str(host / 'deploy'),
              'listen': '0.0.0.0:4792', 'data_dir': str(data), 'tls_dir': str(tls), 'device_id': device,
              'uid': 1001, 'gid': 1001}
    receipts = []
    for settings, expected in ((config, 'activated'), (dict(config, public_url=public_url), 'activated'),
                               (dict(config, public_url=public_url), 'already_active')):
        result = subprocess.run([sys.executable, '-B', str(ROOT / 'scripts/deploy-activate.py'), '--config', '-',
                                 '--image', fixture, '--tag', info['tag'], '--source-sha', info['source_sha'],
                                 '--version', info['version']],
                                input=json.dumps(settings), capture_output=True, text=True)
        release.require(result.returncode == 0, 'rehearsal ' + (result.stderr.strip() or 'activation failed'))
        receipt = json.loads(result.stdout)
        release.require(receipt['status'] == expected and receipt['image'] == fixture, 'rehearsal receipt differs')
        receipts.append(receipt['status'])
    return {'service': 'node', 'tag': info['tag'], 'rehearsal': receipts}


def self_test():
    base = {'DEPLOY_HOST': 'host.example', 'DEPLOY_USER': 'deploy', 'DEPLOY_SSH_KEY': 'fixture',
            'DEPLOY_KNOWN_HOSTS': 'fixture', 'DEPLOY_CONFIG': '{}'}
    assert transport(base) == ('host.example', 'deploy', '22')
    for key, value in (('DEPLOY_HOST', '-oProxyCommand=bad'), ('DEPLOY_USER', 'x;id'),
                       ('DEPLOY_PORT', '0'), ('DEPLOY_PORT', '65536'), ('DEPLOY_SSH_KEY', '')):
        try:
            transport(dict(base, **{key: value}))
        except ValueError:
            pass
        else:
            raise AssertionError('accepted invalid transport')
    print('deployment transport self-test passed')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='action', required=True)
    sub.add_parser('self-test')
    sub.add_parser('rehearse').add_argument('--output', required=True)
    for action in ('build', 'activate'):
        command = sub.add_parser(action)
        command.add_argument('--service', choices=deploy.SERVICES, required=True)
        command.add_argument('--output', required=True)
        if action == 'build':
            command.add_argument('--tag', required=True)
    args = parser.parse_args()
    if args.action == 'self-test':
        self_test()
    elif args.action == 'rehearse':
        print(json.dumps(rehearse(args.output)))
    else:
        info = build(args.service, args.tag, args.output) if args.action == 'build' else activate(args.service, args.output)
        print(json.dumps(info))


if __name__ == '__main__':
    try:
        main()
    except (ValueError, KeyError, TypeError, OSError, subprocess.SubprocessError) as error:
        sys.exit(str(error))
