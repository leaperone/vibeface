"""Late-bound repository root, process boundary and local identity/manifest reads."""
import json
from pathlib import Path
import re
import subprocess
import release_format


ROOT = Path(__file__).resolve().parent.parent


def run(*args, cwd=None):
    return subprocess.check_output(args, cwd=cwd or ROOT, text=True).strip()


def service_installer(target):
    return ROOT / 'scripts' / ('install-node-service.ps1' if target in release_format.WINDOWS_TARGETS else 'install-node-service.sh')


def read_version(app, root=None):
    root = root or ROOT
    release_format.require(app in release_format.APPS, 'unknown application')
    directory = root / 'apps' / app
    if app in ('core', 'cli', 'desktop'):
        match = re.search(r'^version\s*=\s*"([^"]+)"', (directory / 'Cargo.toml').read_text(), re.M)
        release_format.require(match, 'package must own its version')
        return match[1]
    data = json.loads((directory / ('app.json' if app == 'mobile' else 'package.json')).read_text())
    return data['expo']['extra']['releaseVersion'] if app == 'mobile' else data['version']


def identity(app, root=None):
    root = root or ROOT
    value = read_version(app, root)
    return {'app': app, 'version': value, 'tag': release_format.release_tag_for(app, value),
            'source_commit': run('git', 'rev-parse', 'HEAD', cwd=root), 'channel': release_format.version(value)}


def contracts():
    return {'data_epoch': control_constant('DATA_EPOCH'), 'update_protocol': control_constant('UPDATE_PROTOCOL')}


def dependencies(app, root=None, required=False):
    release_format.require(app in release_format.APPS, 'unknown application')
    path = (root or ROOT) / 'releases' / f'{app}.json'
    pins = json.loads(path.read_text()) if path.exists() else {}
    release_format.require(isinstance(pins, dict) and set(pins) <= set(release_format.DEPENDENCIES[app]), 'unexpected dependency')
    if required:
        release_format.require(set(pins) == set(release_format.DEPENDENCIES[app]), f'{app} requires exact dependency pins')
    for dependency, pin in pins.items():
        release_format.validate_pin(dependency, pin)
    return pins


def component_identity(app, root=None):
    release_format.require(app in release_format.COMPONENTS, 'unknown component')
    return identity(app, root)


def control_constant(name):
    match = re.search(rf'pub const {name}: u32 = (\d+);', (ROOT / 'crates/control/src/lib.rs').read_text(encoding='utf-8'))
    release_format.require(match, f'missing {name}')
    return int(match[1])


def git(*args, root=None, data=None, env=None):
    return subprocess.check_output(['git', *args], cwd=root or ROOT, input=data, env=env).decode().strip()


def blob(commit, path, root=None):
    return subprocess.check_output(['git', 'show', f'{commit}:{path}'], cwd=root or ROOT)
