#!/usr/bin/env python3
"""Exercise independent source selection and real archive composition without publication."""
import copy
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import hourly
import release


class ReleaseTrains(unittest.TestCase):
    def test_running_release_is_not_rebuilt_on_the_next_hour(self):
        def git(*args, **kwargs):
            return 'core-v0.0.1-hourly.2026111408' if args[:2] == ('tag', '--list') else 'a' * 40
        with patch.object(hourly, 'git', side_effect=git), patch.object(hourly, 'verify', return_value=('b' * 40, '2026111408')), \
                patch.object(hourly, 'input_digest', return_value='same-inputs'), \
                patch.object(hourly, 'release_runs', return_value=[{'status': 'in_progress', 'conclusion': None}]):
            self.assertEqual(hourly.pending_attempt('core', 'c' * 40, {}),
                             {'status': 'in_progress', 'tag': 'core-v0.0.1-hourly.2026111408'})

    def test_repeated_failed_source_stops_rebuilding(self):
        for count, expected in ((1, None), (2, 'failed_source_requires_fix')):
            tags = '\n'.join('core-v0.0.1-hourly.202611140' + str(8 - index) for index in range(count))
            def git(*args, **kwargs):
                return tags if args[:2] == ('tag', '--list') else 'a' * 40
            with patch.object(hourly, 'git', side_effect=git), patch.object(hourly, 'verify', return_value=('b' * 40, '2026111408')), \
                    patch.object(hourly, 'input_digest', return_value='same-inputs'), \
                    patch.object(hourly, 'release_runs', return_value=[{'status': 'completed', 'conclusion': 'failure'}]):
                state = hourly.pending_attempt('core', 'c' * 40, {})
                self.assertEqual(state['status'] if state else None, expected)

    def test_website_change_releases_only_website(self):
        base = hourly.git('rev-parse', 'HEAD')
        with tempfile.TemporaryDirectory() as temporary:
            env = dict(os.environ, GIT_INDEX_FILE=str(Path(temporary) / 'index'))
            hourly.git('read-tree', base, env=env)
            path = 'apps/www/package.json'
            source = json.loads(hourly.blob(base, path))
            source['description'] = 'Independent Website build input fixture'
            oid = hourly.git('hash-object', '-w', '--stdin', data=json.dumps(source).encode())
            hourly.git('update-index', '--cacheinfo', '100644', oid, path, env=env)
            tree = hourly.git('write-tree', env=env)
            author = dict(os.environ, GIT_AUTHOR_NAME='release-fixture', GIT_COMMITTER_NAME='release-fixture',
                          GIT_AUTHOR_EMAIL=hourly.BOT, GIT_COMMITTER_EMAIL=hourly.BOT)
            changed = hourly.git('commit-tree', tree, '-p', base, '-m', 'Website input fixture', env=author)
            for app in release.APPS:
                self.assertEqual(hourly.input_digest(app, base) == hourly.input_digest(app, changed), app != 'www', app)

    def test_hourly_source_rejects_an_unrelated_change(self):
        base = hourly.git('rev-parse', 'HEAD')
        tree = hourly.expected_tree('core', base, '2026111409')
        identity = dict(os.environ, GIT_AUTHOR_EMAIL=hourly.BOT, GIT_COMMITTER_EMAIL=hourly.BOT,
                        GIT_AUTHOR_NAME='github-actions[bot]', GIT_COMMITTER_NAME='github-actions[bot]')
        source = hourly.git('commit-tree', tree, '-p', base, '-m', 'chore(release): core hourly 2026111409', env=identity)
        self.assertEqual(hourly.verify(source, 'core'), (base, '2026111409'))
        with tempfile.TemporaryDirectory() as temporary:
            env = dict(identity, GIT_INDEX_FILE=str(Path(temporary) / 'index'))
            hourly.git('read-tree', tree, env=env)
            oid = hourly.git('hash-object', '-w', '--stdin', data=b'unrelated modification')
            hourly.git('update-index', '--cacheinfo', '100644', oid, 'README.md', env=env)
            bad_tree = hourly.git('write-tree', env=env)
            bad = hourly.git('commit-tree', bad_tree, '-p', base, '-m', 'chore(release): core hourly 2026111409', env=identity)
            with self.assertRaisesRegex(ValueError, 'exact selected version'):
                hourly.verify(bad, 'core')

    def test_cli_composes_and_runs_different_component_versions(self):
        own_version = '0.0.1-hourly.2026111409'
        cli_source, core_source, app_source = 'a' * 40, 'b' * 40, 'c' * 40
        target = 'x86_64-unknown-linux-gnu'
        own = dict(app='cli', version=own_version, tag='cli-v' + own_version, source_commit=cli_source, channel='hourly')
        def pin(app, value, source):
            return dict(tag=f'{app}-v{value}', version=value, source_commit=source, manifest_sha256='d' * 64)
        pins = {'core': pin('core', '2.3.4', core_source), 'app': pin('app', '3.4.5-rc.2', app_source)}
        original_run = release.run
        def boundary(*args, **kwargs):
            if args[0] == 'git':
                if args[1:] == ('status', '--porcelain'):
                    return ''
                if args[1:] == ('rev-parse', 'HEAD'):
                    return cli_source
            return original_run(*args, **kwargs)
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            native = root / 'native'
            native.mkdir()
            binary = native / 'vibeface'
            binary.write_text('#!/bin/sh\nprintf "vibeface ' + own_version + '\\n"\n')
            binary.chmod(0o755)
            release.write_json(native / 'NATIVE-INFO.json', dict(schema=1, target=target, version=own_version,
                source_commit=cli_source, data_epoch=1, files=release.files(native)))
            archives = {}
            for app, dependency in pins.items():
                directory = root / app
                directory.mkdir()
                if app == 'core':
                    core = directory / 'vibeface-core'
                    core.write_text('#!/bin/sh\nprintf "vibeface-core 2.3.4\\n"\n')
                    core.chmod(0o755)
                else:
                    (directory / 'web').mkdir()
                    (directory / 'web/index.html').write_text('<html>App 3.4.5-rc.2</html>')
                component = {key: dependency[key] for key in ('tag', 'version', 'source_commit')}
                component |= dict(app=app, schema=1, target=target if app == 'core' else None,
                                  channel=release.version(dependency['version']), files=release.files(directory))
                if app == 'core':
                    component['data_epoch'] = 1
                release.write_json(directory / 'COMPONENT-INFO.json', component)
                archive = root / (app + '.tar.gz')
                release.public_archive(directory, archive)
                archives[app] = archive
            with patch.object(release, 'identity', return_value=own), patch.object(release, 'dependencies', return_value=pins), \
                    patch.object(release, 'read_version', return_value=own_version), patch.object(release, 'run', side_effect=boundary):
                release.compose_offline(target, native, archives['app'], root / 'output', archives['core'])
                manifest = release.verify_manifest(root / 'output', own)
                package = release.unpack(root / 'output' / f'vibeface-{own_version}-{target}.tar.gz', root / 'extracted')
                build = release.verify_bundle(package, target, manifest)
                self.assertEqual(build['core_version'], '2.3.4')
                self.assertEqual(build['app_source_commit'], app_source)
                self.assertEqual(subprocess.check_output([str(package / 'vibeface-core'), '--version'], text=True).strip(), 'vibeface-core 2.3.4')
                self.assertEqual((package / 'web/index.html').read_text(), '<html>App 3.4.5-rc.2</html>')
                foreign = copy.deepcopy(manifest)
                foreign['dependencies']['app']['source_commit'] = 'e' * 40
                with self.assertRaisesRegex(ValueError, 'dependency differs'):
                    release.verify_bundle(package, target, foreign)
                (package / 'vibeface-core').write_text('modified bytes')
                with self.assertRaisesRegex(ValueError, 'sealed file'):
                    release.verify_bundle(package, target, manifest)


if __name__ == '__main__':
    unittest.main()
