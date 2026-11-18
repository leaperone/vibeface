#!/usr/bin/env python3
"""Build and package the Windows CLI, Core and browser application without a POSIX shell."""

import sys
sys.dont_write_bytecode = True
import argparse
import os
from pathlib import Path
import subprocess
import tempfile

sys.path.insert(0, str(Path(__file__).resolve().parent))

import release_archive
import release_assembly
import release_context
import release_format


TARGET = 'x86_64-pc-windows-msvc'


def helper(*args):
    subprocess.run([sys.executable, '-B', str(release_context.ROOT / 'scripts' / args[0]), *args[1:]], cwd=release_context.ROOT, check=True)


def build(target_dir):
    flags = subprocess.check_output([sys.executable, '-B', str(release_context.ROOT / 'scripts/release-rustflags.py')],
                                    cwd=release_context.ROOT, text=True).strip()
    flags += '\x1f-C\x1ftarget-feature=+crt-static'
    subprocess.run(['cargo', 'build', '--release', '--locked', '-p', 'vibeface', '-p', 'vibeface-core',
                    '--target', TARGET], cwd=release_context.ROOT, check=True,
                   env=dict(os.environ, CARGO_ENCODED_RUSTFLAGS=flags))
    binaries = [target_dir / TARGET / 'release' / name for name in release_format.program_names(TARGET)]
    helper('verify-windows-imports.py', *(str(binary) for binary in binaries))
    return binaries


def package_windows(output):
    version = release_context.read_version('cli')
    web = release_assembly.prepare_node_web()
    binaries = build(Path(os.environ.get('CARGO_TARGET_DIR', release_context.ROOT / 'target')))
    output.mkdir(parents=True, exist_ok=True)
    name = f'vibeface-{version}-{TARGET}'
    with tempfile.TemporaryDirectory(dir=output, prefix='.package-') as tmp:
        archive = output / f'{name}.tar.gz'
        package = Path(tmp) / name
        release_assembly.stage_node_binaries(package, TARGET, *binaries)
        release_assembly.archive_local_node(package, TARGET, web, archive)
    print(f'Package: {archive}\nChecksum: {archive}.sha256')
    return archive


def self_test():
    from unittest.mock import patch
    web_files = release_assembly.node_web_files()
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
        for name in web_files:
            (web / name).write_text('fixture\n')
        binaries = []
        for name in release_format.program_names(TARGET):
            binary = tmp / name
            value = versions['core' if name == 'vibeface-core.exe' else 'cli']
            binary.write_text(f'#!/bin/sh\necho "{name.removesuffix(".exe")} {value}"\n')
            binary.chmod(0o755)
            binaries.append(binary)
        package = tmp / f'vibeface-{version}-{TARGET}'
        release_assembly.stage_node_binaries(package, TARGET, *binaries)
        release_assembly.stage_node_assets(package, TARGET, web, license=release_context.ROOT / 'LICENSE')
        requirements = (package / 'RUNTIME-REQUIREMENTS.md').read_bytes()
        assert requirements == (release_archive.RUNTIME_REQUIREMENTS + release_archive.WINDOWS_RUNTIME_REQUIREMENTS).encode('utf-8')
        assert b'\r' not in requirements and requirements.endswith(b'\n')
        assert b'official local Agent CLI' in requirements and b'nor WSL, nor Developer Mode' in requirements
        names = {path.relative_to(package).as_posix() for path in package.rglob('*') if path.is_file()}
        assert names == {'vibeface.exe', 'vibeface-core.exe', 'README.md', 'RUNTIME-REQUIREMENTS.md',
                         'scripts/install-node-service.ps1', *release_assembly.offline_docs().sources().values(),
                         *(f'web/{name}' for name in web_files),
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
                release_assembly.stage_node_binaries(missing, TARGET, *binaries)
                release_assembly.stage_node_assets(missing, TARGET, web)
            except ValueError as error:
                assert 'service installer' in str(error), error
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
