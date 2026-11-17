#!/usr/bin/env python3
"""Build and package the Windows CLI, Core and browser application without a POSIX shell."""

import sys
sys.dont_write_bytecode = True
import argparse
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

sys.path.insert(0, str(Path(__file__).resolve().parent))

import release_archive
import release_context
import release_format


TARGET = 'x86_64-pc-windows-msvc'
WEB_FILES = ('_shell.html', 'index.html', '_redirects', '_headers', '.vibeface-build.json',
             'vibeface_secure.js', 'vibeface_secure_bg.wasm')
import importlib.util
spec = importlib.util.spec_from_file_location('offline_docs', release_context.ROOT / 'scripts/offline-docs.py')
offline_docs = importlib.util.module_from_spec(spec)
spec.loader.exec_module(offline_docs)


def helper(*args):
    subprocess.run([sys.executable, '-B', str(release_context.ROOT / 'scripts' / args[0]), *args[1:]], cwd=release_context.ROOT, check=True)


def build(target_dir):
    helper('build-inputs.py', 'check', 'crypto')
    helper('build-inputs.py', 'check', 'web')
    flags = subprocess.check_output([sys.executable, '-B', str(release_context.ROOT / 'scripts/release-rustflags.py')],
                                    cwd=release_context.ROOT, text=True).strip()
    flags += '\x1f-C\x1ftarget-feature=+crt-static'
    subprocess.run(['cargo', 'build', '--release', '--locked', '-p', 'vibeface', '-p', 'vibeface-core',
                    '--target', TARGET], cwd=release_context.ROOT, check=True,
                   env=dict(os.environ, CARGO_ENCODED_RUSTFLAGS=flags))
    binaries = [target_dir / TARGET / 'release' / name for name in release_format.program_names(TARGET)]
    helper('verify-windows-imports.py', *(str(binary) for binary in binaries))
    return binaries


def stage(package, binaries, web):
    package.mkdir(parents=True)
    for binary in binaries:
        release_format.require(binary.is_file(), f'missing release binary: {binary.name}')
        shutil.copy2(binary, package / binary.name)
    shutil.copytree(web, package / 'web')
    installer = release_context.service_installer(TARGET)
    release_format.require(installer.is_file(), f'missing Windows service installer: scripts/{installer.name}')
    (package / 'scripts').mkdir()
    shutil.copy2(installer, package / 'scripts' / installer.name)
    offline_docs.assemble(package)
    if (release_context.ROOT / 'LICENSE').is_file():
        shutil.copy2(release_context.ROOT / 'LICENSE', package / 'LICENSE')
    release_archive.write_runtime_requirements(package, TARGET)
    return package


def package_windows(output):
    version = release_context.read_version('cli')
    web = release_context.ROOT / 'apps/app/dist/client'
    for name in WEB_FILES:
        release_format.require((web / name).is_file() and (web / name).stat().st_size > 0,
                        'Build the browser application and its cryptography before packaging')
    binaries = build(Path(os.environ.get('CARGO_TARGET_DIR', release_context.ROOT / 'target')))
    output.mkdir(parents=True, exist_ok=True)
    name = f'vibeface-{version}-{TARGET}'
    with tempfile.TemporaryDirectory(dir=output, prefix='.package-') as tmp:
        package = stage(Path(tmp) / name, binaries, web)
        os.environ['PACKAGE_TARGET'] = TARGET
        build_info = release_archive.seal_bundle(package)
        release_format.verify_build_info(build_info, release_context.contracts())
        archive = output / f'{name}.tar.gz'
        release_archive.public_archive(package, archive)
    print(f'Package: {archive}\nChecksum: {archive}.sha256')
    return archive


def self_test():
    from unittest.mock import patch
    scratch = release_context.ROOT / '.artifacts/windows-package-tests'
    scratch.mkdir(parents=True, exist_ok=True)
    versions = {'cli': '0.0.1-hourly.2026111409', 'core': '2.3.4', 'app': '3.4.5-rc.2'}
    version = versions['cli']
    source = 'a' * 40
    installers = scratch / 'service-installers'
    installers.mkdir(exist_ok=True)
    (installers / 'install-node-service.ps1').write_text('# service installer fixture\n')

    def boundary(*args, **kwargs):
        if args[0] == 'git':
            return source if args[1] == 'rev-parse' else ''
        raise AssertionError(args)

    with tempfile.TemporaryDirectory(dir=scratch) as tmp, \
            patch('release_context.run', side_effect=boundary), \
            patch('release_context.read_version', side_effect=versions.__getitem__), \
            patch('release_context.service_installer', return_value=installers / 'install-node-service.ps1'):
        tmp = Path(tmp)
        web = tmp / 'web'
        web.mkdir()
        for name in WEB_FILES:
            (web / name).write_text('fixture\n')
        binaries = []
        for name in release_format.program_names(TARGET):
            binary = tmp / name
            value = versions['core' if name == 'vibeface-core.exe' else 'cli']
            binary.write_text(f'#!/bin/sh\necho "{name.removesuffix(".exe")} {value}"\n')
            binary.chmod(0o755)
            binaries.append(binary)
        package = stage(tmp / f'vibeface-{version}-{TARGET}', binaries, web)
        requirements = (package / 'RUNTIME-REQUIREMENTS.md').read_bytes()
        assert requirements == (release_archive.RUNTIME_REQUIREMENTS + release_archive.WINDOWS_RUNTIME_REQUIREMENTS).encode('utf-8')
        assert b'\r' not in requirements and requirements.endswith(b'\n')
        assert b'official local Agent CLI' in requirements and b'nor WSL, nor Developer Mode' in requirements
        names = {path.relative_to(package).as_posix() for path in package.rglob('*') if path.is_file()}
        assert names == {'vibeface.exe', 'vibeface-core.exe', 'README.md', 'RUNTIME-REQUIREMENTS.md',
                         'scripts/install-node-service.ps1', *offline_docs.sources().values(),
                         *(f'web/{name}' for name in WEB_FILES),
                         *(['LICENSE'] if (release_context.ROOT / 'LICENSE').is_file() else [])}, sorted(names)
        os.environ['PACKAGE_TARGET'] = TARGET
        core = {'version': '2.3.4', 'source_commit': 'b' * 40}
        app = {'version': '3.4.5-rc.2', 'source_commit': 'c' * 40}
        info = release_archive.seal_bundle(package, core, app)
        release_format.verify_build_info(info, release_context.contracts())
        assert len(info) == 9 and info['target'] == TARGET and info['source_commit'] == source
        identity = {'version': version, 'source_commit': source, 'dependencies': {'core': core, 'app': app}}
        assert release_archive.verify_bundle(package, TARGET, identity) == info
        sealed = (package / 'FILES.sha256').read_text(encoding='utf-8').splitlines()
        assert {line.split('  ')[1] for line in sealed} == names | {'BUILD-INFO.json'}, sorted(sealed)
        assert (package / 'FILES.sha256').read_bytes() == ('\n'.join(sealed) + '\n').encode('utf-8'), \
            'the sealed manifest must use canonical UTF-8/LF bytes on every build host'
        archive = tmp / 'first' / f'vibeface-{version}-{TARGET}.tar.gz'
        release_archive.public_archive(package, archive)
        assert archive.with_name(archive.name + '.sha256').read_text(encoding='utf-8') == f'{release_archive.digest(archive)}  {archive.name}\n'
        for path in (package, *package.rglob('*')):
            os.utime(path, (1800000000, 1800000000))
            path.chmod(0o751 if path.is_dir() or path.stat().st_mode & 0o111 else 0o664)
        second = tmp / 'second' / archive.name
        release_archive.public_archive(package, second)
        assert archive.read_bytes() == second.read_bytes()
        assert archive.with_name(archive.name + '.sha256').read_bytes() == second.with_name(second.name + '.sha256').read_bytes()
        for number, output in enumerate((archive, second)):
            helper('checksum.py', str(output.with_name(output.name + '.sha256')))
            unpacked = release_archive.unpack(output, tmp / f'unpacked-{number}')
            assert release_archive.verify_bundle(unpacked, TARGET, identity) == info
            assert (unpacked / 'RUNTIME-REQUIREMENTS.md').read_bytes() == requirements
        missing = tmp / 'missing-installer'
        with patch('release_context.service_installer', return_value=installers / 'absent.ps1'):
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
    parser.add_argument('--output', type=Path, default=release_context.ROOT / '.artifacts/packages')
    parser.add_argument('--self-test', action='store_true')
    args = parser.parse_args()
    self_test() if args.self_test else package_windows(args.output)


if __name__ == '__main__':
    main()
