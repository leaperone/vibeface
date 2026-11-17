"""Local release manifests, complete asset verification and producer collection."""
import json
from pathlib import Path
import shutil
import tempfile
import release_archive
import release_context
import release_format


def manifest(app, output):
    output = Path(output)
    info = dict(release_context.identity(app), schema=release_format.SCHEMA_VERSION, contracts=release_context.contracts(), dependencies=release_context.dependencies(app, required=True),
                assets=[{'name': p.name, 'size': p.stat().st_size, 'sha256': release_archive.digest(p)}
                        for p in sorted(output.iterdir()) if p.is_file() and p.name != 'release.json'])
    release_format.verify_release_identity(info, release_context.contracts())
    release_archive.write_json(output / 'release.json', info)
    return info


def verify_manifest(directory, expected=None):
    directory = Path(directory)
    info = release_format.verify_release_identity(json.loads((directory / 'release.json').read_text()), release_context.contracts())
    if expected:
        release_format.require(all(info[k] == value for k, value in expected.items()), 'release manifest pin mismatch')
    names = {asset['name'] for asset in info['assets']}
    release_format.require(names == {p.name for p in directory.iterdir()} - {'release.json'}, 'unexpected release files')
    for asset in info['assets']:
        path = directory / asset['name']
        release_format.require(path.is_file() and not path.is_symlink() and path.stat().st_size == asset['size']
                and release_archive.digest(path) == asset['sha256'], 'release asset checksum mismatch')
    return info


def verify_release(app, directory, expected=None):
    directory = Path(directory)
    info = verify_manifest(directory, expected)
    release_format.require(info['app'] == app, 'release application differs')
    signed = app == 'mobile' and (directory / f'vibeface-{info["version"]}-ios.ipa').is_file()
    release_format.require({p.name for p in directory.iterdir()} == release_format.expected_asset_names(app, info['version'], signed), 'incomplete release asset set')
    pins = {key: info[key] for key in ('version', 'tag', 'source_commit', 'channel')}
    for asset in info['assets']:
        name = asset['name']
        if name + '.sha256' in {p.name for p in directory.iterdir()}:
            release_archive.verify_checksum_file(directory, name)
        if name.endswith('.tar.gz'):
            with tempfile.TemporaryDirectory() as temp:
                unpacked = release_archive.unpack(directory / name, Path(temp) / 'unpack')
                if app == 'cli':
                    target = next(target for target in release_format.TARGETS if name.endswith(target + '.tar.gz'))
                    release_archive.verify_bundle(unpacked, target, info)
                elif app in release_format.COMPONENTS:
                    release_archive.verify_component(unpacked, app, pins)
    if app == 'desktop':
        for target in release_format.TARGETS:
            release_archive.verify_desktop_receipt(directory, target, pins)
    if app == 'mobile':
        name = f'vibeface-{info["version"]}-android.apk'
        receipt = json.loads((directory / (name + '.provenance.json')).read_text())
        release_format.require(receipt == release_archive.mobile_provenance(directory / name), 'Android provenance differs')
        release_archive.verify_ios_assets(directory, pins)
    return info


def collect(app, input_root, output):
    input_root, output = Path(input_root), Path(output)
    expected = release_context.identity(app)
    output.mkdir(parents=True, exist_ok=True)
    release_format.require(not any(output.iterdir()), 'collection output must be empty')
    for source in sorted(input_root.rglob('release.json')):
        part = verify_manifest(source.parent, expected)
        release_format.require(part['dependencies'] == release_context.dependencies(app, required=True), 'producer dependency pins differ')
        for asset in part['assets']:
            path = output / asset['name']
            release_format.require(not path.exists(), 'producers must own distinct asset names')
            shutil.copy2(source.parent / path.name, path)
    manifest(app, output)
    return verify_release(app, output, expected)
