#!/usr/bin/env python3
"""Real producer directories for progressive publication tests (local fixtures only)."""
import json
import shutil
import subprocess
import tempfile
from pathlib import Path
from unittest.mock import patch

import release as rel
from release import (
    DESKTOP_TARGETS, ROOT, TARGETS, compose_offline, manifest, pack_component, program_names,
    require_version_cohort, stage_native, version,
)


def boundary_run(source, badging, installers):
    def runner(*args, **kwargs):
        if args[0] == 'git':
            if args[1] == 'rev-parse':
                return source
            if args[1] == 'status':
                return ''
        if args[0] == 'aapt':
            return badging
        return subprocess.check_output(args, text=True).strip()
    return runner


def fixture_installer(installers, target):
    suffix = '.ps1' if target.endswith('-pc-windows-msvc') else '.sh'
    return installers / f'install-node-service{suffix}'


def native_programs(directory, target, reported):
    directory.mkdir(parents=True, exist_ok=True)
    for name in program_names(target):
        binary = directory / name
        binary.write_text(f'#!/bin/sh\necho "{name.removesuffix(".exe")} {reported}"\n', encoding='utf-8')
        binary.chmod(0o755)
    return directory


def stage_desktop(api, destination, target, product, runtime_archive, *, content="fixture payload", **changes):
    """Use the caller's patched release module, including when it runs as __main__."""
    destination.mkdir(parents=True, exist_ok=True)
    artifacts = {}
    for name in api.desktop_payload_names(product['version'], target):
        payload = destination / name
        payload.write_bytes(f'{name} {content}'.encode())
        payload.with_name(name + '.sha256').write_text(f'{api.digest(payload)}  {name}\n', encoding='utf-8')
        artifacts[name] = api.digest(payload)
    receipt = dict(product, schema=api.DESKTOP_SCHEMA, app='desktop', target=target, **api.NATIVE_UI,
        signed=target == 'aarch64-apple-darwin',
        signing='developer-id-notarized' if target == 'aarch64-apple-darwin'
        else 'platform-checksums' if target.endswith('-unknown-linux-gnu') else 'unsigned-development-preview',
        runtime={'archive': runtime_archive.name, 'sha256': api.digest(runtime_archive)}, artifacts=artifacts)
    receipt.update(changes)
    api.write_json(destination / f"{api.desktop_basename(product['version'], target)}.provenance.json", receipt)
    return destination


def build_collected_tree(source=None):
    """Return (collected_path, product_identity) with app + one native + one desktop producer."""
    source = source or 'a' * 40
    product_version = require_version_cohort(('core', 'app'))
    product_tag = f'v{product_version}'
    channel = version(product_version)
    expo = json.loads((ROOT / 'apps/mobile/app.json').read_text(encoding='utf-8'))['expo']
    badging = (f"package: name='{expo['android']['package']}' versionCode='{expo['android']['versionCode']}'"
               f" versionName='{expo['version']}'")
    scratch = ROOT / '.artifacts/progressive-publication-tests'
    scratch.mkdir(parents=True, exist_ok=True)
    installers = scratch / 'installers'
    installers.mkdir(exist_ok=True)
    for name in ('install-node-service.sh', 'install-node-service.ps1'):
        (installers / name).write_text('# fixture\n', encoding='utf-8')
    tmp = Path(tempfile.mkdtemp(dir=scratch))
    collected = tmp / 'collected'
    run = boundary_run(source, badging, installers)
    with patch('release.run', side_effect=run), patch('release.shutil.which', return_value='aapt'), \
            patch('release.service_installer', side_effect=lambda t: fixture_installer(installers, t)):
        product = dict(app='product', version=product_version, tag=product_tag,
                       source_commit=source, channel=channel)
        web_source = tmp / 'web'
        web_source.mkdir(parents=True)
        (web_source / 'vibeface_secure.js').write_text('export const s = 1;\n', encoding='utf-8')
        (web_source / 'vibeface_secure_bg.wasm').write_bytes(b'\x00asm')
        (web_source / 'index.html').write_text('<html></html>\n', encoding='utf-8')
        app_out = tmp / 'app-out'
        pack_component('app', web_source, app_out)
        app_archive = app_out / f'vibeface-app-{product_version}.tar.gz'
        target = TARGETS[0]
        native_bin = tmp / 'native-bin'
        native_programs(native_bin, target, product_version)
        stage_native(target, native_bin, tmp / 'native-staged')
        compose_out = tmp / 'native-compose'
        compose_offline(target, tmp / 'native-staged', app_archive, compose_out)
        collected.mkdir()
        shutil.copytree(app_out, collected / 'app')
        (collected / 'native' / target).mkdir(parents=True)
        for name in compose_out.iterdir():
            if name.is_file():
                shutil.copy2(name, collected / 'native' / target / name.name)
        manifest('product', collected / 'native' / target)
        desktop_target = DESKTOP_TARGETS[0]
        desktop_dir = collected / 'desktop' / desktop_target
        runtime = collected / 'native' / desktop_target / f'vibeface-{product_version}-{desktop_target}.tar.gz'
        stage_desktop(rel, desktop_dir, desktop_target, product, runtime, content='desktop fixture')
        manifest('product', desktop_dir)
        return collected, product
