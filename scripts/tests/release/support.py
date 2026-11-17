"""Isolated, credential-free subprocess fixtures for release contracts."""
import base64
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import zipfile

SOURCE = Path(__file__).resolve().parents[2]
BASE = 'e2093b537da3ba0b6f3fed057d81ab60bc54dd51'
MODULES = ('release_component_identity', 'release_format', 'release_context', 'release_source', 'release_archive',
           'release_inventory', 'release_github', 'release_publisher', 'release_assembly')
TARGETS = ('aarch64-apple-darwin', 'aarch64-unknown-linux-gnu',
           'x86_64-unknown-linux-gnu', 'x86_64-pc-windows-msvc')
CLI = '0.0.1-hourly.2026111409'
VERSIONS = {'core': '2.3.4', 'app': '3.4.5-rc.2', 'cli': CLI,
            'desktop': '4.5.6-hourly.2026111409', 'mobile': CLI, 'www': '5.6.7'}
CONTRACTS = {'data_epoch': 1, 'update_protocol': 1}
REPO = 'leaperone/vibeface'


def write_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + '\n')


def pin(app, value=None, source=None, sha=None):
    value = value or VERSIONS[app]
    return dict(tag=f'{app}-v{value}', version=value,
                source_commit=source or {'core': 'b', 'app': 'c', 'cli': 'a'}[app] * 40,
                manifest_sha256=sha or {'core': 'd', 'app': 'e', 'cli': 'f'}[app] * 64)


def environment(root, **extra):
    # Deliberately allowlist environment; no host tokens, provider configuration,
    # signing material, Python startup overrides, or host Git config is inherited.
    env = {'PATH': str(root / 'bin'), 'HOME': str(root / 'home'),
           'LANG': 'C.UTF-8', 'LC_ALL': 'C.UTF-8',
           'RELEASE_FIXTURE': str(root), 'GIT_CONFIG_NOSYSTEM': '1',
           'PYTHONPATH': str(root / 'network-guard.zip')}
    env.update(extra)
    return env


class Fixture:
    def __init__(self, baseline=False):
        self.temporary = tempfile.TemporaryDirectory(prefix='release-offline-')
        self.root = Path(self.temporary.name)
        self.scripts = self.root / 'scripts'
        self.scripts.mkdir()
        (self.root / 'bin').mkdir()
        (self.root / 'home').mkdir()
        if baseline:
            for name in ('release.py', 'hourly.py'):
                content = subprocess.check_output([shutil.which('git'), 'show', f'{BASE}:scripts/{name}'],
                                                  cwd=SOURCE.parent, env=environment(self.root))
                (self.scripts / name).write_bytes(content)
        else:
            for name in ('release', *MODULES):
                shutil.copy2(SOURCE / (name + '.py'), self.scripts / (name + '.py'))
            shutil.copytree(SOURCE / 'tests', self.scripts / 'tests',
                            ignore=shutil.ignore_patterns('__pycache__'))
        (self.scripts / 'offline-docs.py').write_text(
            'def assemble(directory):\n    (directory / "docs").mkdir()\n'
            '    (directory / "docs/guide.md").write_text("public guide\\n")\n')
        for name in ('install-node-service.sh', 'install-node-service.ps1'):
            (self.scripts / name).write_text('# service fixture\n')
        for app, value in VERSIONS.items():
            path = self.root / 'apps' / app
            path.mkdir(parents=True)
            if app in ('core', 'cli', 'desktop'):
                (path / 'Cargo.toml').write_text(f'[package]\nname = "vibeface-{app}"\nversion = "{value}"\n')
            elif app == 'mobile':
                write_json(path / 'app.json', {'expo': {'version': '1.0.0', 'extra': {'releaseVersion': value},
                    'android': {'package': 'cn.leaperone.vibeface', 'versionCode': 26111409},
                    'ios': {'bundleIdentifier': 'cn.leaperone.vibeface', 'buildNumber': '2611.14.9'}}})
            else:
                write_json(path / 'package.json', {'version': value})
        control = self.root / 'crates/control/src/lib.rs'
        control.parent.mkdir(parents=True)
        control.write_text('pub const DATA_EPOCH: u32 = 1;\npub const UPDATE_PROTOCOL: u32 = 1;\n')
        write_json(self.root / 'releases/cli.json', {'core': pin('core'), 'app': pin('app')})
        write_json(self.root / 'releases/desktop.json', {'cli': pin('cli')})
        external = Path(__file__).with_name('fake_external.py').read_text()
        for tool in ('git', 'gh', 'aapt'):
            path = self.root / 'bin' / tool
            path.write_text('#!' + sys.executable + '\n' + external)
            path.chmod(0o755)
        self.save({'head': 'a' * 40, 'events': [], 'releases': {}, 'on_main': ['a' * 40]})
        with zipfile.ZipFile(self.root / 'network-guard.zip', 'w') as guard:
            guard.writestr('sitecustomize.py',
                'import socket\n'
                'def denied(*args, **kwargs): raise AssertionError("network forbidden in release fixture")\n'
                'socket.socket.connect = denied\n'
                'socket.socket.connect_ex = denied\n'
                'socket.socket.sendto = denied\n'
                'socket.create_connection = denied\n'
                'socket.getaddrinfo = denied\n')
        self.env = environment(self.root)

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.temporary.cleanup()

    def state(self):
        return json.loads((self.root / 'state.json').read_text())

    def save(self, state):
        write_json(self.root / 'state.json', state)

    def change(self, **values):
        state = self.state()
        state.update(values)
        self.save(state)

    def cli(self, *args, code=0, bytecode_flag=True, **environment_values):
        command = [sys.executable]
        if bytecode_flag:
            command.append('-B')
        result = subprocess.run([*command, str(self.scripts / 'release.py'), *map(str, args)],
                                cwd=self.root, env=dict(self.env, **environment_values),
                                capture_output=True, text=True)
        if result.returncode != code:
            raise AssertionError((args, result.returncode, result.stdout, result.stderr))
        return result

    def native(self, app, value=None, directory='native-input', target=None):
        suffix = '.exe' if target == 'x86_64-pc-windows-msvc' else ''
        name = ('vibeface-core' if app == 'core' else 'vibeface') + suffix
        path = self.root / directory / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text('#!' + sys.executable + '\nimport sys\n'
            'if sys.argv[1:] != ["--version"]: raise SystemExit(98)\n'
            f'print({("vibeface-core" if app == "core" else "vibeface") + " " + (value or VERSIONS[app])!r})\n')
        path.chmod(0o755)
        return path

    def static(self, app='app'):
        directory = self.root / ('static-' + app)
        directory.mkdir(exist_ok=True)
        (directory / 'index.html').write_bytes(b'fixture app\n')
        return directory

    def component(self, app='app', target=None, output=None):
        source = self.native('core', target=target) if app == 'core' else self.static(app)
        output = self.root / (output or ('out-' + app + ('-' + target if target else '')))
        args = ['component', app, '--input', source, '--output', output]
        if target:
            args += ['--target', target]
        self.cli(*args)
        return output

    def remote(self, directory):
        info = json.loads((directory / 'release.json').read_text())
        assets = []
        for i, path in enumerate(directory.iterdir(), 1):
            if path.is_file():
                import hashlib
                body = path.read_bytes()
                assets.append({'id': i, 'name': path.name, 'size': len(body),
                    'digest': 'sha256:' + hashlib.sha256(body).hexdigest(),
                    'body': base64.b64encode(body).decode()})
        record = {'tag_name': info['tag'], 'draft': False, 'prerelease': info['channel'] != 'stable',
                  'assets': assets, 'source': info['source_commit']}
        state = self.state()
        state['releases'][info['tag']] = record
        self.save(state)
        return info


def fixture_repository():
    """Create a disposable Git object database; never mutate the project repository."""
    temporary = tempfile.TemporaryDirectory(prefix='release-source-git-')
    root = Path(temporary.name)
    selected = ('Cargo.toml', 'Cargo.lock', 'README.md', 'README.zh-CN.md', 'apps/core/Cargo.toml', 'apps/cli/Cargo.toml',
                'apps/desktop/Cargo.toml', 'apps/app/package.json', 'apps/www/package.json',
                'apps/mobile/app.json', 'crates/control/src/lib.rs')
    for name in selected:
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(SOURCE.parent / name, path)
    shutil.copytree(SOURCE.parent / 'packages/docs', root / 'packages/docs')
    shutil.copytree(SOURCE, root / 'scripts', ignore=shutil.ignore_patterns('__pycache__', 'node_modules'))
    git = shutil.which('git')
    env = {'PATH': str(Path(git).parent), 'HOME': str(root / 'home'), 'GIT_CONFIG_NOSYSTEM': '1',
           'GIT_AUTHOR_NAME': 'release-fixture', 'GIT_COMMITTER_NAME': 'release-fixture',
           'GIT_AUTHOR_EMAIL': 'fixture@example.invalid', 'GIT_COMMITTER_EMAIL': 'fixture@example.invalid',
           'LANG': 'C.UTF-8'}
    for args in (('init', '-q'), ('add', '.'), ('commit', '-qm', 'fixture')):
        subprocess.run([git, '-c', 'core.hooksPath=/dev/null', *args], cwd=root, env=env,
                       check=True, capture_output=True)
    return temporary, root, env
