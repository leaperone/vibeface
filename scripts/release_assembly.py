"""Component staging and offline CLI composition before inventory creation."""
import os
from pathlib import Path
import shutil
import tempfile
import release_archive
import release_context
import release_format
import release_inventory


def pack_component(app, source, output, target=None):
    source, output = Path(source), Path(output)
    info = release_context.component_identity(app)
    release_format.require(not release_context.run('git', 'status', '--porcelain'), 'release source must be clean')
    if app == 'core':
        release_format.require(target in release_format.TARGETS and source.is_file(), 'Core requires a target binary')
        release_format.require(source.name == release_format.program_names(target)[1], 'Core binary name must match its platform')
        release_format.require(release_context.run(str(source.resolve()), '--version') == 'vibeface-core ' + info['version'], 'Core binary version mismatch')
    else:
        release_format.require(source.is_dir() and target is None, 'static component requires a directory')
    name = f'vibeface-{app}-{info["version"]}' + (f'-{target}' if target else '')
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=output, prefix='.component-') as tmp:
        directory = Path(tmp) / name
        directory.mkdir()
        if app == 'core':
            shutil.copy2(source, directory / release_format.program_names(target)[1])
        else:
            shutil.copytree(source, directory / ('web' if app == 'app' else 'site'))
        payload = dict(info, schema=release_format.SCHEMA_VERSION, target=target, files=release_archive.files(directory))
        if app == 'core':
            payload['data_epoch'] = release_context.control_constant('DATA_EPOCH')
        release_archive.write_json(directory / 'COMPONENT-INFO.json', payload)
        release_archive.verify_component(directory, app)
        release_archive.public_archive(directory, output / f'{name}.tar.gz')
    release_inventory.manifest(app, output)


def stage_native(target, binary_directory, output, app='cli'):
    release_format.require(app == 'cli' and target in release_format.TARGETS, 'native staging owns CLI only')
    info = release_context.identity('cli')
    binary = Path(binary_directory) / release_format.program_names(target)[0]
    release_format.require(binary.is_file() and not binary.is_symlink(), 'CLI binary is missing')
    release_format.require(release_context.run(str(binary.resolve()), '--version') == 'vibeface ' + info['version'], 'CLI binary version mismatch')
    release_format.require(not release_context.run('git', 'status', '--porcelain'), 'release source must be clean')
    output = Path(output)
    output.mkdir(parents=True, exist_ok=True)
    release_format.require(not any(output.iterdir()), 'native output must be empty')
    shutil.copy2(binary, output / binary.name)
    release_archive.write_json(output / 'NATIVE-INFO.json', dict(schema=release_format.SCHEMA_VERSION, target=target, version=info['version'],
               source_commit=info['source_commit'], files=release_archive.files(output), data_epoch=release_context.control_constant('DATA_EPOCH')))
    return release_archive.native_receipt(output, target, info['version'], info['source_commit'])


def compose_offline(target, native_dir, app_archive, output, core_archive):
    release_format.require(target in release_format.TARGETS, 'unsupported platform')
    info = release_context.identity('cli')
    release_format.require(not release_context.run('git', 'status', '--porcelain'), 'release source must be clean')
    native_dir, output = Path(native_dir), Path(output)
    release_archive.native_receipt(native_dir, target, info['version'], info['source_commit'])
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=output, prefix='.compose-') as temporary:
        tmp = Path(temporary)
        app_root = release_archive.unpack(Path(app_archive), tmp / 'app')
        core_root = release_archive.unpack(Path(core_archive), tmp / 'core')
        app_info = release_archive.verify_component(app_root, 'app')
        core_info = release_archive.verify_component(core_root, 'core', {'target': target})
        pins = release_context.dependencies('cli')
        if pins:
            for app, component in (('core', core_info), ('app', app_info)):
                release_format.require(component['tag'] == pins[app]['tag'] and component['source_commit'] == pins[app]['source_commit'], 'bundle dependency pin mismatch')
        name = f'vibeface-{info["version"]}-{target}'
        directory = tmp / name
        directory.mkdir()
        cli, core = release_format.program_names(target)
        shutil.copy2(native_dir / cli, directory / cli)
        shutil.copy2(core_root / core, directory / core)
        shutil.copytree(app_root / 'web', directory / 'web')
        (directory / 'scripts').mkdir()
        installer = release_context.service_installer(target)
        shutil.copy2(installer, directory / 'scripts' / installer.name)
        import importlib.util
        spec = importlib.util.spec_from_file_location('offline_docs', release_context.ROOT / 'scripts/offline-docs.py')
        docs = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(docs)
        docs.assemble(directory)
        release_archive.write_runtime_requirements(directory, target)
        os.environ['PACKAGE_TARGET'] = target
        release_archive.seal_bundle(directory, core_info, app_info)
        release_archive.public_archive(directory, output / (name + '.tar.gz'))
    release_inventory.manifest('cli', output)
