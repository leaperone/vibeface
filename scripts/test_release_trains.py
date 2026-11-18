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
from datetime import datetime
from io import StringIO
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

    def test_node_release_inputs_follow_actual_consumers(self):
        for name in ('release-contract.mjs', 'release-metadata.mjs'):
            self.assert_changed_inputs('scripts/' + name, {'cli', 'desktop'})
        for name in ('windows-signing.mjs', 'private-control.mjs'):
            self.assert_changed_inputs('scripts/' + name, {'core', 'cli', 'desktop'})
        self.assert_changed_inputs('scripts/sealed-runtime.mjs', {'desktop'})
        self.assert_changed_inputs('scripts/test-release-contract.mjs', set())
        self.assert_changed_inputs('scripts/fixtures/release-contract.json', set())

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

    def test_core_coordinator_never_schedules_clients_or_website(self):
        with (patch.object(hourly, 'require_coordinator_context'),
              patch.object(release_context, 'git'),
              patch.object(hourly, 'green_main', return_value='a' * 40),
              patch.object(hourly, 'latest_release_tags', return_value={}) as latest,
              patch.object(hourly, 'reconcile_app', return_value={'status': 'unchanged'}) as reconcile,
              patch('sys.stdout', new_callable=StringIO)):
            result = hourly.run_hourly(datetime.fromisoformat('2026-11-18T02:17:00Z'))
        self.assertEqual(result, {'core': {'status': 'unchanged'}})
        latest.assert_called_once_with(('core',))
        reconcile.assert_called_once_with('core', 'a' * 40, '2026111802', {}, None)
        with self.assertRaisesRegex(ValueError, 'no scheduled release'):
            hourly.reconcile_app('www', 'a' * 40, '2026111802', {}, None)

    def test_client_cutoff_follows_los_angeles_and_uses_the_scheduled_hour(self):
        for created, expected in (
            ('2026-11-18T02:00:00Z', '2026-11-18T02:00:00+00:00'),
            ('2026-11-18T03:19:00Z', '2026-11-18T02:00:00+00:00'),
            ('2026-07-18T01:06:00Z', '2026-07-18T01:00:00+00:00'),
        ):
            with self.subTest(created=created):
                self.assertEqual(hourly.client_cutoff(datetime.fromisoformat(created)).isoformat(), expected)

    def test_client_snapshot_uses_only_core_manifests_completed_by_the_cutoff(self):
        cutoff = datetime.fromisoformat('2026-11-18T02:00:00Z')
        def record(app, batch, created_at, draft=False):
            return {'tag_name': f'{app}-v0.0.1-hourly.{batch}', 'draft': draft,
                    'assets': [{'name': 'release.json', 'created_at': created_at}]}

        records = [
            record('core', '2026111802', '2026-11-18T02:18:00Z'),
            record('core', '2026111801', '2026-11-18T01:42:00Z', draft=True),
            record('core', '2026111800', '2026-11-18T01:20:00Z'),
            record('app', '2026111802', '2026-11-18T02:12:00Z'),
            record('www', '2026111801', '2026-11-18T01:42:00Z'),
        ]
        with patch.object(hourly, 'gitless_gh', return_value=json.dumps([records])):
            self.assertEqual(hourly.latest_release_tags(('core', *hourly.CLIENT_APPS), core_cutoff=cutoff),
                             {'core': records[2]['tag_name'], 'app': records[3]['tag_name']})

    def test_daily_rerun_recovers_source_from_generated_tags_and_rejects_stale_batches(self):
        cutoff = datetime.fromisoformat('2026-11-18T02:00:00Z')
        base = release_context.git('rev-parse', 'HEAD')
        release_context.git('update-ref', 'refs/remotes/origin/main', base)
        with patch.object(hourly, 'green_main') as green:
            for batch in ('2026111802', '2026111902'):
                tree = release_source.expected_tree('mobile', base, batch)
                identity = dict(os.environ, GIT_AUTHOR_EMAIL=release_source.BOT, GIT_COMMITTER_EMAIL=release_source.BOT)
                source = release_context.git('commit-tree', tree, '-p', base, '-m',
                                             f'chore(release): mobile hourly {batch}', env=identity)
                version = release_source.hourly_version('mobile', base, batch)
                tag = f'mobile-v{version}'
                release_context.git('tag', tag, source)
                self.addCleanup(release_context.git, 'tag', '-d', tag)
                if batch == '2026111802':
                    self.assertEqual(hourly.client_source(cutoff, batch), base)
            with self.assertRaisesRegex(ValueError, 'newer client batch'):
                hourly.client_source(cutoff, '2026111802')
        green.assert_not_called()

    def test_older_client_run_cannot_publish_after_a_newer_schedule_without_tags(self):
        original = {'created_at': '2026-11-18T02:07:00Z', 'workflow_id': 99}
        latest = {'workflow_runs': [{'created_at': '2026-11-19T02:04:00Z'}]}
        with (patch.dict(os.environ, {'GITHUB_EVENT_NAME': 'schedule', 'GITHUB_RUN_ID': '7'}),
              patch.object(hourly, 'require_coordinator_context'),
              patch.object(hourly, 'api', side_effect=[original, latest]),
              patch.object(release_context, 'git') as git):
            with self.assertRaisesRegex(ValueError, 'newer scheduled client batch'):
                hourly.run_clients()
        git.assert_not_called()

    def test_client_batch_waits_for_exact_manifests_and_preserves_unrelated_releases(self):
        from tests.release.support import pin

        base = 'a' * 40
        cutoff = datetime.fromisoformat('2026-11-18T02:00:00Z')
        version = '0.0.1-hourly.2026111802'
        core = pin('core', value='0.0.1-hourly.2026111801')
        app_pin = pin('app', value=version)
        for failed_app in (None, 'app', 'cli'):
            with self.subTest(failed_app=failed_app):
                cli_pin = pin('cli', value='0.0.1-hourly.2026111702' if failed_app == 'cli' else version)
                newest = {'core': core['tag']}
                if failed_app == 'cli':
                    newest['cli'] = cli_pin['tag']
                # A queued release may already have a public GitHub Release without its final manifest.
                attempts = {}
                started = []

                def observe(app, tag, source):
                    self.assertEqual(tag, newest.get(app, f'{app}-v{version}'))
                    self.assertEqual(source, base)
                    attempts[app] = attempts.get(app, 0) + 1
                    return [{'status': 'completed' if attempts[app] > 1 else 'in_progress',
                             'conclusion': 'failure' if app == failed_app else 'success',
                             'html_url': 'https://example.invalid/' + app}]

                def api(path, *, missing=False):
                    if path == 'actions/runs/7':
                        return {'created_at': '2026-11-18T02:07:00Z', 'workflow_id': 99}
                    if path == 'actions/workflows/99/runs?branch=main&event=schedule&per_page=1':
                        return {'workflow_runs': [{'created_at': '2026-11-18T02:07:00Z'}]}
                    tag = path.removeprefix('releases/tags/')
                    app = release_format.parse_tag(tag)[0]
                    self.assertTrue(missing)
                    complete = attempts[app] > 1 and not (failed_app == 'app' and app == 'app')
                    return {'draft': False, 'assets': [{'name': 'release.json'}] if complete else []}

                def reconcile(app, source, batch, pins, previous):
                    self.assertEqual((source, batch, previous), (base, '2026111802', newest.get(app)))
                    expected = {'core': core, 'app': app_pin} if app == 'cli' else {'cli': cli_pin} if app == 'desktop' else {}
                    self.assertEqual(pins, expected)
                    if previous:
                        return {'status': 'unchanged', 'tag': previous}
                    started.append(app)
                    return {'status': 'dispatched', 'tag': f'{app}-v{version}'}

                publications = {item['tag']: item for item in (core, app_pin, cli_pin)}
                with (patch.dict(os.environ, {'GH_TOKEN': 'offline', 'GITHUB_REPOSITORY': hourly.REPO,
                                             'GITHUB_REF': 'refs/heads/main', 'GITHUB_EVENT_NAME': 'schedule',
                                             'GITHUB_RUN_ID': '7'}),
                      patch.object(hourly, 'api', side_effect=api),
                      patch.object(release_context, 'git'),
                      patch.object(hourly, 'green_main', return_value=base) as green,
                      patch.object(hourly, 'latest_release_tags', return_value=newest) as latest,
                      patch.object(hourly, 'published_pin', side_effect=lambda tag: publications.get(tag, {})),
                      patch.object(hourly, 'reconcile_app', side_effect=reconcile),
                      patch.object(hourly, 'tag_commit', return_value=base),
                      patch.object(hourly, 'release_runs', side_effect=observe),
                      patch.object(hourly.time, 'sleep'),
                      patch('sys.stdout', new_callable=StringIO) as logs):
                    if failed_app:
                        with self.assertRaisesRegex(ValueError, 'did not complete'):
                            hourly.run_clients()
                        self.assertIn('"mobile": {"status": "released"', logs.getvalue())
                        self.assertIn(f'"{failed_app}": {{"status": "failed"', logs.getvalue())
                    else:
                        result = hourly.run_clients()
                        self.assertEqual({app: outcome['status'] for app, outcome in result.items()},
                                         {app: 'released' for app in ('app', 'cli', 'desktop', 'mobile')})
                green.assert_called_once_with(cutoff)
                latest.assert_called_once_with(('core', 'app', 'cli', 'desktop', 'mobile'), core_cutoff=cutoff)
                consumers = {None: ['cli', 'desktop'], 'app': [], 'cli': ['desktop']}
                self.assertEqual(started, ['app', 'mobile', *consumers[failed_app]])

    def test_client_retry_observes_the_existing_daily_tag_and_never_replaces_it(self):
        base = release_context.git('rev-parse', 'HEAD')
        batch = '2026111802'
        version = release_source.hourly_version('app', base, batch)
        tag = f'app-v{version}'
        source = 'b' * 40
        with (patch.object(hourly, 'tag_commit', return_value=source),
              patch.object(release_source, 'verify', return_value=(base, batch)),
              patch.object(hourly, 'reconcile') as write,
              patch.object(hourly, 'release_runs',
                           return_value=[{'html_url': 'https://example.invalid/app'}]) as runs):
            result = hourly.reconcile_app('app', base, batch, {}, None)
        self.assertEqual(result, {'status': 'dispatched', 'tag': tag, 'runs': ['https://example.invalid/app']})
        runs.assert_called_once_with('app', tag, source)
        write.assert_not_called()

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
                patch.object(hourly, 'is_ancestor', return_value=True), \
                patch.object(hourly, 'input_digest', return_value='same-inputs'), \
                patch.object(hourly, 'release_runs', return_value=[{'status': 'in_progress', 'conclusion': None}]):
            self.assertEqual(hourly.pending_attempt('core', 'c' * 40, '2026111409', {}),
                             {'status': 'in_progress', 'tag': 'core-v0.0.1-hourly.2026111408'})

    def test_repeated_failed_source_stops_rebuilding(self):
        for count, expected in ((1, None), (2, 'failed_source_requires_fix')):
            tags = '\n'.join('core-v0.0.1-hourly.202611140' + str(8 - index) for index in range(count))
            def git(*args, **kwargs):
                return tags if args[:2] == ('tag', '--list') else 'a' * 40
            with patch.object(release_context, 'git', side_effect=git), patch.object(release_source, 'verify', return_value=('b' * 40, '2026111408')), \
                    patch.object(hourly, 'is_ancestor', return_value=True), \
                    patch.object(hourly, 'input_digest', return_value='same-inputs'), \
                    patch.object(hourly, 'release_runs', return_value=[{'status': 'completed', 'conclusion': 'failure'}]):
                state = hourly.pending_attempt('core', 'c' * 40, '2026111409', {})
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
