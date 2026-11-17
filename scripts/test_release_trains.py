#!/usr/bin/env python3
"""Exercise independent source selection and real archive composition without publication."""

import sys
sys.dont_write_bytecode = True
import release_archive
import release_assembly
import release_context
import release_format
import release_inventory
import release_source
import copy
import json
import os
from pathlib import Path
import runpy
import subprocess
import tempfile
import unittest
import zipfile
from unittest.mock import patch

import hourly


class ReleaseTrains(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        from tests.release.support import fixture_repository
        cls.fixture_temporary, cls.fixture_root, cls.fixture_environment = fixture_repository()
        cls.root_patch = patch.object(release_context, 'ROOT', cls.fixture_root)
        cls.environment_patch = patch.dict(os.environ, cls.fixture_environment, clear=True)
        cls.root_patch.start()
        cls.environment_patch.start()

    @classmethod
    def tearDownClass(cls):
        cls.environment_patch.stop()
        cls.root_patch.stop()
        cls.fixture_temporary.cleanup()

    def test_release_owners_are_common_inputs_and_tests_are_not(self):
        from tests.release.support import MODULES
        for module in MODULES:
            self.assert_changed_inputs('scripts/' + module + '.py', set(release_format.APPS))
        self.assert_changed_inputs('scripts/tests/release/test_cli.py', set())

    def test_portable_desktop_zip_accepts_epoch_runtime_files(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / 'application'
            binary = source / 'runtime/vibeface-core'
            binary.parent.mkdir(parents=True)
            binary.write_bytes(b'signed runtime bytes')
            binary.chmod(0o755)
            os.utime(binary, (0, 0))
            output = root / 'desktop.zip'
            result = subprocess.run([sys.executable, str(release_context.ROOT / 'scripts/zip-directory.py'),
                                     str(source), str(output)], capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            with zipfile.ZipFile(output) as archive:
                self.assertEqual(archive.namelist(), ['runtime/vibeface-core'])
                self.assertEqual(archive.read('runtime/vibeface-core'), b'signed runtime bytes')
                info = archive.getinfo('runtime/vibeface-core')
                self.assertEqual(info.date_time, (1980, 1, 1, 0, 0, 0))
                self.assertEqual((info.external_attr >> 16) & 0o777, 0o755)
            self.assertEqual(binary.stat().st_mtime, 0)
            self.assertEqual(binary.read_bytes(), b'signed runtime bytes')

    def test_desktop_zip_helper_is_a_desktop_build_input(self):
        self.assert_changed_inputs('scripts/zip-directory.py', {'desktop'})

    def test_coordinator_changes_do_not_rebuild_applications(self):
        self.assert_changed_inputs('scripts/hourly.py', set())

    def assert_changed_inputs(self, path, affected):
        base = release_context.git('rev-parse', 'HEAD')
        with tempfile.TemporaryDirectory() as temporary:
            env = dict(os.environ, GIT_INDEX_FILE=str(Path(temporary) / 'index'))
            release_context.git('read-tree', base, env=env)
            oid = release_context.git('hash-object', '-w', '--stdin', data=release_context.blob(base, path) + b'\n# input fixture\n')
            release_context.git('update-index', '--cacheinfo', '100644', oid, path, env=env)
            tree = release_context.git('write-tree', env=env)
            author = dict(os.environ, GIT_AUTHOR_NAME='release-fixture', GIT_COMMITTER_NAME='release-fixture',
                          GIT_AUTHOR_EMAIL=release_source.BOT, GIT_COMMITTER_EMAIL=release_source.BOT)
            changed = release_context.git('commit-tree', tree, '-p', base, '-m', 'Release input fixture', env=author)
            for app in release_format.APPS:
                self.assertEqual(hourly.input_digest(app, base) != hourly.input_digest(app, changed), app in affected, app)

    def test_npm_bootstrap_ignores_unrelated_release_channels(self):
        choose = runpy.run_path(str(release_context.ROOT / 'scripts/verify-node.py'))['_npm_cli_candidate']
        registry = {'versions': {'0.0.1-nightly.20261112.1': {}, '0.0.1-rc.1': {}}}
        records = [[{'tag_name': 'cli-v0.0.1-nightly.20261112.1', 'draft': False, 'assets': []},
                    {'tag_name': 'cli-v0.0.1', 'draft': False, 'prerelease': False, 'assets': [{'name': 'release.json'}]},
                    {'tag_name': 'cli-v0.0.1-hourly.2026111409', 'draft': False, 'prerelease': True, 'assets': []}]]
        self.assertIsNone(choose(registry, records))

    def test_npm_baseline_selects_completed_registry_intersection(self):
        choose = runpy.run_path(str(release_context.ROOT / 'scripts/verify-node.py'))['_npm_cli_candidate']
        old, new = '0.0.1-hourly.2026111408', '0.0.1-hourly.2026111409'
        registry = {'versions': {old: {}, '0.0.1-nightly.20261112.1': {}}}
        completed = lambda value: {'tag_name': 'cli-v' + value, 'draft': False, 'prerelease': True,
                                   'assets': [{'name': 'release.json'}]}
        self.assertEqual(choose(registry, [[completed(new), completed(old)]]), (old, 'cli-v' + old))
        incomplete = completed(old) | {'assets': []}
        with self.assertRaisesRegex(AssertionError, 'pending or draft'):
            choose(registry, [[incomplete]])

    def test_running_release_is_not_rebuilt_on_the_next_hour(self):
        def git(*args, **kwargs):
            return 'core-v0.0.1-hourly.2026111408' if args[:2] == ('tag', '--list') else 'a' * 40
        with patch.object(release_context, 'git', side_effect=git), patch.object(release_source, 'verify', return_value=('b' * 40, '2026111408')), \
                patch.object(hourly, 'input_digest', return_value='same-inputs'), \
                patch.object(hourly, 'release_runs', return_value=[{'status': 'in_progress', 'conclusion': None}]):
            self.assertEqual(hourly.pending_attempt('core', 'c' * 40, {}),
                             {'status': 'in_progress', 'tag': 'core-v0.0.1-hourly.2026111408'})

    def test_repeated_failed_source_stops_rebuilding(self):
        for count, expected in ((1, None), (2, 'failed_source_requires_fix')):
            tags = '\n'.join('core-v0.0.1-hourly.202611140' + str(8 - index) for index in range(count))
            def git(*args, **kwargs):
                return tags if args[:2] == ('tag', '--list') else 'a' * 40
            with patch.object(release_context, 'git', side_effect=git), patch.object(release_source, 'verify', return_value=('b' * 40, '2026111408')), \
                    patch.object(hourly, 'input_digest', return_value='same-inputs'), \
                    patch.object(hourly, 'release_runs', return_value=[{'status': 'completed', 'conclusion': 'failure'}]):
                state = hourly.pending_attempt('core', 'c' * 40, {})
                self.assertEqual(state['status'] if state else None, expected)

    def test_website_change_releases_only_website(self):
        base = release_context.git('rev-parse', 'HEAD')
        with tempfile.TemporaryDirectory() as temporary:
            env = dict(os.environ, GIT_INDEX_FILE=str(Path(temporary) / 'index'))
            release_context.git('read-tree', base, env=env)
            path = 'apps/www/package.json'
            source = json.loads(release_context.blob(base, path))
            source['description'] = 'Independent Website build input fixture'
            oid = release_context.git('hash-object', '-w', '--stdin', data=json.dumps(source).encode())
            release_context.git('update-index', '--cacheinfo', '100644', oid, path, env=env)
            tree = release_context.git('write-tree', env=env)
            author = dict(os.environ, GIT_AUTHOR_NAME='release-fixture', GIT_COMMITTER_NAME='release-fixture',
                          GIT_AUTHOR_EMAIL=release_source.BOT, GIT_COMMITTER_EMAIL=release_source.BOT)
            changed = release_context.git('commit-tree', tree, '-p', base, '-m', 'Website input fixture', env=author)
            for app in release_format.APPS:
                self.assertEqual(hourly.input_digest(app, base) == hourly.input_digest(app, changed), app != 'www', app)

    def test_hourly_source_rejects_an_unrelated_change(self):
        base = release_context.git('rev-parse', 'HEAD')
        tree = release_source.expected_tree('core', base, '2026111409')
        identity = dict(os.environ, GIT_AUTHOR_EMAIL=release_source.BOT, GIT_COMMITTER_EMAIL=release_source.BOT,
                        GIT_AUTHOR_NAME='github-actions[bot]', GIT_COMMITTER_NAME='github-actions[bot]')
        source = release_context.git('commit-tree', tree, '-p', base, '-m', 'chore(release): core hourly 2026111409', env=identity)
        self.assertEqual(release_source.verify(source, 'core'), (base, '2026111409'))
        with tempfile.TemporaryDirectory() as temporary:
            env = dict(identity, GIT_INDEX_FILE=str(Path(temporary) / 'index'))
            release_context.git('read-tree', tree, env=env)
            oid = release_context.git('hash-object', '-w', '--stdin', data=b'unrelated modification')
            release_context.git('update-index', '--cacheinfo', '100644', oid, 'README.md', env=env)
            bad_tree = release_context.git('write-tree', env=env)
            bad = release_context.git('commit-tree', bad_tree, '-p', base, '-m', 'chore(release): core hourly 2026111409', env=identity)
            with self.assertRaisesRegex(ValueError, 'exact selected version'):
                release_source.verify(bad, 'core')

    def test_cli_composes_and_runs_different_component_versions(self):
        own_version = '0.0.1-hourly.2026111409'
        cli_source, core_source, app_source = 'a' * 40, 'b' * 40, 'c' * 40
        target = 'x86_64-unknown-linux-gnu'
        own = dict(app='cli', version=own_version, tag='cli-v' + own_version, source_commit=cli_source, channel='hourly')
        def pin(app, value, source):
            return dict(tag=f'{app}-v{value}', version=value, source_commit=source, manifest_sha256='d' * 64)
        pins = {'core': pin('core', '2.3.4', core_source), 'app': pin('app', '3.4.5-rc.2', app_source)}
        original_run = release_context.run
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
            release_archive.write_json(native / 'NATIVE-INFO.json', dict(schema=1, target=target, version=own_version,
                source_commit=cli_source, data_epoch=1, files=release_archive.files(native)))
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
                                  channel=release_format.version(dependency['version']), files=release_archive.files(directory))
                if app == 'core':
                    component['data_epoch'] = 1
                release_archive.write_json(directory / 'COMPONENT-INFO.json', component)
                archive = root / (app + '.tar.gz')
                release_archive.public_archive(directory, archive)
                archives[app] = archive
            with patch.object(release_context, 'identity', return_value=own), patch.object(release_context, 'dependencies', return_value=pins), \
                    patch.object(release_context, 'read_version', return_value=own_version), patch.object(release_context, 'run', side_effect=boundary):
                release_assembly.compose_offline(target, native, archives['app'], root / 'output', archives['core'])
                manifest = release_inventory.verify_manifest(root / 'output', own)
                package = release_archive.unpack(root / 'output' / f'vibeface-{own_version}-{target}.tar.gz', root / 'extracted')
                build = release_archive.verify_bundle(package, target, manifest)
                self.assertEqual(build['core_version'], '2.3.4')
                self.assertEqual(build['app_source_commit'], app_source)
                self.assertEqual(subprocess.check_output([str(package / 'vibeface-core'), '--version'], text=True).strip(), 'vibeface-core 2.3.4')
                self.assertEqual((package / 'web/index.html').read_text(), '<html>App 3.4.5-rc.2</html>')
                self.assertEqual((package / 'RUNTIME-REQUIREMENTS.md').read_bytes(), release_archive.RUNTIME_REQUIREMENTS.encode('utf-8'))
                foreign = copy.deepcopy(manifest)
                foreign['dependencies']['app']['source_commit'] = 'e' * 40
                with self.assertRaisesRegex(ValueError, 'dependency differs'):
                    release_archive.verify_bundle(package, target, foreign)
                (package / 'vibeface-core').write_text('modified bytes')
                with self.assertRaisesRegex(ValueError, 'sealed file'):
                    release_archive.verify_bundle(package, target, manifest)


if __name__ == '__main__':
    unittest.main()
