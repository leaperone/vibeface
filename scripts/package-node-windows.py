#!/usr/bin/env python3
"""Build and package the Windows CLI, Core and browser application without a POSIX shell."""
import argparse
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parent))

import release  # noqa: E402  the packaging identity, sealing and archive rules live in one place

TARGET = 'x86_64-pc-windows-msvc'
ROOT = release.ROOT
WEB_FILES = ('_shell.html', 'index.html', '_redirects', '_headers', '.vibeface-build.json',
             'vibeface_secure.js', 'vibeface_secure_bg.wasm')
DOCS = ('node.md', 'releasing.md')
REQUIREMENTS = """# Runtime requirements

The archive contains the CLI, Core and static browser application for Windows. Install the provider
tools separately before enabling the bot. No credentials or provider runtimes are included.

Run `scripts/install-node-service.ps1` from Windows PowerShell to register the per-user service. The
installer needs neither administrator rights, nor WSL, nor Developer Mode.
"""


def helper(*args):
    subprocess.run([sys.executable, '-B', str(ROOT / 'scripts' / args[0]), *args[1:]], cwd=ROOT, check=True)


def build(target_dir):
    helper('build-inputs.py', 'check', 'crypto')
    helper('build-inputs.py', 'check', 'web')
    flags = subprocess.check_output([sys.executable, '-B', str(ROOT / 'scripts/release-rustflags.py')],
                                    cwd=ROOT, text=True).strip()
    subprocess.run(['cargo', 'build', '--release', '--locked', '-p', 'vibeface', '-p', 'vibeface-core',
                    '--target', TARGET], cwd=ROOT, check=True,
                   env=dict(os.environ, CARGO_ENCODED_RUSTFLAGS=flags))
    return [target_dir / TARGET / 'release' / name for name in release.program_names(TARGET)]


def stage(package, binaries, web):
    package.mkdir(parents=True)
    for binary in binaries:
        release.require(binary.is_file(), f'missing release binary: {binary.name}')
        shutil.copy2(binary, package / binary.name)
    shutil.copytree(web, package / 'web')
    installer = release.service_installer(TARGET)
    release.require(installer.is_file(), f'missing Windows service installer: scripts/{installer.name}')
    (package / 'scripts').mkdir()
    shutil.copy2(installer, package / 'scripts' / installer.name)
    shutil.copy2(ROOT / 'README.md', package / 'README.md')
    if (ROOT / 'LICENSE').is_file():
        shutil.copy2(ROOT / 'LICENSE', package / 'LICENSE')
    (package / 'docs').mkdir()
    for name in DOCS:
        shutil.copy2(ROOT / 'packages/docs' / name, package / 'docs' / name)
    (package / 'RUNTIME-REQUIREMENTS.md').write_text(REQUIREMENTS)
    return package


def package_windows(output):
    version = release.require_version_cohort(('core', 'app'))
    web = ROOT / 'apps/app/dist/client'
    for name in WEB_FILES:
        release.require((web / name).is_file() and (web / name).stat().st_size > 0,
                        'Build the browser application and its cryptography before packaging')
    binaries = build(Path(os.environ.get('CARGO_TARGET_DIR', ROOT / 'target')))
    output.mkdir(parents=True, exist_ok=True)
    name = f'vibeface-{version}-{TARGET}'
    with tempfile.TemporaryDirectory(dir=output, prefix='.package-') as tmp:
        package = stage(Path(tmp) / name, binaries, web)
        os.environ['PACKAGE_TARGET'] = TARGET
        build_info = release.seal_bundle(package)
        release.verify_build_info(build_info)
        archive = output / f'{name}.tar.gz'
        release.public_archive(package, archive)
    print(f'Package: {archive}\nChecksum: {archive}.sha256')
    return archive


def self_test():
    from unittest.mock import patch
    scratch = ROOT / '.artifacts/windows-package-tests'
    scratch.mkdir(parents=True, exist_ok=True)
    version = release.canonical_product_version()
    source = 'a' * 40
    installers = scratch / 'service-installers'
    installers.mkdir(exist_ok=True)
    (installers / 'install-node-service.ps1').write_text('# service installer fixture\n')

    def boundary(*args, **kwargs):
        if args[0] == 'git':
            return source if args[1] == 'rev-parse' else ''
        raise AssertionError(args)

    with tempfile.TemporaryDirectory(dir=scratch) as tmp, \
            patch('release.run', side_effect=boundary), \
            patch(f'{__name__}.release.service_installer', return_value=installers / 'install-node-service.ps1'):
        tmp = Path(tmp)
        web = tmp / 'web'
        web.mkdir()
        for name in WEB_FILES:
            (web / name).write_text('fixture\n')
        binaries = []
        for name in release.program_names(TARGET):
            binary = tmp / name
            binary.write_text(f'#!/bin/sh\necho "{name.removesuffix(".exe")} {version}"\n')
            binary.chmod(0o755)
            binaries.append(binary)
        package = stage(tmp / f'vibeface-{version}-{TARGET}', binaries, web)
        names = {path.relative_to(package).as_posix() for path in package.rglob('*') if path.is_file()}
        assert names == {'vibeface.exe', 'vibeface-core.exe', 'README.md', 'RUNTIME-REQUIREMENTS.md',
                         'scripts/install-node-service.ps1', *(f'docs/{name}' for name in DOCS),
                         *(f'web/{name}' for name in WEB_FILES),
                         *(['LICENSE'] if (ROOT / 'LICENSE').is_file() else [])}, sorted(names)
        os.environ['PACKAGE_TARGET'] = TARGET
        info = release.seal_bundle(package)
        release.verify_build_info(info)
        assert len(info) == 9 and info['target'] == TARGET and info['source_commit'] == source
        sealed = (package / 'FILES.sha256').read_text().splitlines()
        assert {line.split('  ')[1] for line in sealed} == names | {'BUILD-INFO.json'}, sorted(sealed)
        archive = tmp / f'vibeface-{version}-{TARGET}.tar.gz'
        release.public_archive(package, archive)
        assert archive.with_name(archive.name + '.sha256').read_text() == f'{release.digest(archive)}  {archive.name}\n'
        missing = tmp / 'missing-installer'
        with patch(f'{__name__}.release.service_installer', return_value=installers / 'absent.ps1'):
            try:
                stage(missing, binaries, web)
            except ValueError as error:
                assert 'Windows service installer' in str(error), error
            else:
                raise AssertionError('accepted a package without its Windows service installer')
    print('Windows packaging self-test passed: .exe basenames, complete payload, nine sealed BUILD-INFO fields '
          'and a reproducible public archive')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, default=ROOT / '.artifacts/packages')
    parser.add_argument('--self-test', action='store_true')
    args = parser.parse_args()
    self_test() if args.self_test else package_windows(args.output)


if __name__ == '__main__':
    main()
