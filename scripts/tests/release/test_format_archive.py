"""tests.release.test format archive ownership boundary."""
import sys
sys.dont_write_bytecode = True
from pathlib import Path
import tempfile
import release_archive
import release_context
import release_format
import release_inventory


def self_test():
    from unittest.mock import patch
    import copy
    for app in release_format.APPS:
        assert release_format.parse_tag(f'{app}-v0.0.1') == (app, '0.0.1', 'stable')
        assert release_format.parse_tag(f'{app}-v0.0.1-hourly.2026111409') == (app, '0.0.1-hourly.2026111409', 'hourly')
    def rejects(action):
        try:
            action()
        except (ValueError, KeyError, TypeError):
            return
        raise AssertionError('invalid release was accepted')
    for tag in ('v0.0.1', 'product-v0.0.1', 'core-v1.0.0-nightly.1', 'cli-v1.0.0-hourly.2026111424'):
        rejects(lambda: release_format.parse_tag(tag))
    build = dict(version='0.0.1-hourly.2026111409', target='x86_64-unknown-linux-gnu', source_commit='a' * 40,
                 core_version='0.0.1-hourly.2026111408', core_source_commit='b' * 40,
                 app_version='0.0.1-rc.2', app_source_commit='c' * 40, **{'data_epoch': 1, 'update_protocol': 1})
    assert release_format.verify_build_info(build, {'data_epoch': 1, 'update_protocol': 1}) == build
    rejects(lambda: release_format.verify_build_info(build | {'data_epoch': 2}, {'data_epoch': 1, 'update_protocol': 1}))
    rejects(lambda: release_format.verify_build_info(build | {'core_source_commit': 'invalid'}, {'data_epoch': 1, 'update_protocol': 1}))
    def pin(app, version_value, source):
        return {'tag': f'{app}-v{version_value}', 'version': version_value, 'source_commit': source, 'manifest_sha256': 'd' * 64}
    pins = {'core': pin('core', build['core_version'], 'b' * 40), 'app': pin('app', build['app_version'], 'c' * 40)}
    own = dict(app='cli', tag='cli-v' + build['version'], version=build['version'], source_commit='a' * 40, channel='hourly')
    with tempfile.TemporaryDirectory() as temporary:
        tmp = Path(temporary)
        package = tmp / 'package'
        package.mkdir()
        release_archive.write_json(package / 'BUILD-INFO.json', build)
        (package / 'vibeface').write_text('CLI bytes')
        (package / 'vibeface-core').write_text('Core bytes')
        (package / 'FILES.sha256').write_text(''.join(f'{value}  {name}\n' for name, value in release_archive.files(package).items()))
        info = own | {'schema': 1, 'contracts': {'data_epoch': 1, 'update_protocol': 1}, 'dependencies': pins,
                      'assets': [{'name': 'fixture.tar.gz', 'size': 1, 'sha256': 'd' * 64}]}
        assert release_format.verify_release_identity(info, {'data_epoch': 1, 'update_protocol': 1}) == info
        assert release_archive.verify_bundle(package, build['target'], info) == build
        bad = copy.deepcopy(info)
        bad['dependencies']['core']['version'] = '1.0.0'
        rejects(lambda: release_archive.verify_bundle(package, build['target'], bad))
        (package / 'vibeface-core').write_text('tampered bytes')
        rejects(lambda: release_archive.verify_bundle(package, build['target'], info))
        bad = copy.deepcopy(info)
        bad['assets'].append(bad['assets'][0])
        rejects(lambda: release_format.verify_release_identity(bad, {'data_epoch': 1, 'update_protocol': 1}))
        bad = copy.deepcopy(info)
        bad['dependencies']['core']['manifest_sha256'] = 'invalid'
        rejects(lambda: release_format.verify_release_identity(bad, {'data_epoch': 1, 'update_protocol': 1}))
        archive = tmp / 'a.tar.gz'
        release_archive.public_archive(package, archive)
        assert (release_archive.unpack(archive, tmp / 'unpacked') / 'vibeface').read_text() == 'CLI bytes'
        for app in ('core', 'app', 'www'):
            base = dict(app=app, version='0.0.1', tag=app + '-v0.0.1', source_commit='a' * 40, channel='stable')
            out = tmp / app
            out.mkdir()
            (out / 'fixture').write_text('fixture')
            with patch('release_context.identity', return_value=base), patch('release_context.dependencies', return_value={}):
                release_inventory.manifest(app, out)
                assert release_inventory.verify_manifest(out)['app'] == app
                (out / 'fixture').write_text('changed')
                rejects(lambda: release_inventory.verify_manifest(out))
    print('Independent release self-test passed: six tags, independent pins, sealed bytes and tampering rejection')
