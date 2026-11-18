#!/usr/bin/env python3
"""Offline shared identity corpus, independent byte checks and image import closure."""
import ast
import builtins
import copy
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
SCRIPTS = Path(__file__).resolve().parents[2]
ROOT = SCRIPTS.parent
sys.path.insert(0, str(SCRIPTS))
import release_archive
import release_component_identity as identity
import release_context
import release_format
import release_inventory


def load_runtime(path, name='component_identity_test_runtime'):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


runtime = load_runtime(ROOT / 'deploy/runtime.py')
CORPUS = json.loads(Path(__file__).with_name('component_identity_corpus.json').read_text())


class ComponentIdentity(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix='component-identity-')
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        # Only the release consumer owns the repository's expected epoch.
        self.epoch = patch.object(release_context, 'control_constant', return_value=1)
        self.epoch.start()
        self.addCleanup(self.epoch.stop)

    def fixture(self, app='core', target='x86_64-unknown-linux-gnu'):
        directory = self.root / app
        directory.mkdir(exist_ok=True)
        name = release_format.program_names(target)[1] if app == 'core' else (
            'web/index.html' if app == 'app' else 'site/index.html')
        payload = directory / name
        payload.parent.mkdir(parents=True, exist_ok=True)
        payload.write_bytes(b'component fixture')
        info = dict(app=app, schema=1, version='0.0.1', tag=app + '-v0.0.1', channel='stable',
                    source_commit='a' * 40, target=target if app == 'core' else None,
                    files={name: hashlib.sha256(payload.read_bytes()).hexdigest()})
        if app == 'core':
            info['data_epoch'] = 1
        self.write(directory, info)
        return directory, info

    def write(self, directory, info):
        (directory / 'COMPONENT-INFO.json').write_text(json.dumps(info))

    def check_case(self, directory, app, info, valid):
        self.write(directory, info)
        checks = (
            lambda: identity.validate_component(info),
            lambda: release_archive.verify_component(directory, app),
            lambda: runtime.component_info(directory),
            lambda: runtime.component_info(directory, verify=False),
        )
        for check in checks:
            if valid:
                self.assertEqual(check(), info)
            else:
                with self.assertRaises(ValueError):
                    check()

    def test_shared_corpus(self):
        for app in identity.COMPONENTS:
            directory, base = self.fixture(app)
            for row in CORPUS['versions']:
                with self.subTest(app=app, case=row['name']):
                    info = copy.deepcopy(base)
                    info.update(version=row['version'], tag=f"{app}-v{row['version']}", channel=row['channel'])
                    self.check_case(directory, app, info, row['valid'])
            rows = CORPUS['fields'] + (CORPUS['core_fields'] if app == 'core' else [])
            for row in rows:
                with self.subTest(app=app, case=row['name']):
                    self.check_case(directory, app, dict(base, **row['changes']), row['valid'])
            for field in CORPUS['missing_fields'] + (['data_epoch'] if app == 'core' else []):
                with self.subTest(app=app, missing=field):
                    info = copy.deepcopy(base)
                    del info[field]
                    self.check_case(directory, app, info, False)
            for info in CORPUS['invalid_documents']:
                with self.subTest(app=app, document=info):
                    self.check_case(directory, app, info, False)

    def test_role_policy_remains_independent(self):
        for target in release_format.TARGETS:
            with self.subTest(target=target), tempfile.TemporaryDirectory(dir=self.root) as temporary:
                with patch.object(self, 'root', Path(temporary)):
                    directory, info = self.fixture(target=target)
                    self.assertEqual(release_archive.verify_component(directory, 'core'), info)
                    if target == 'x86_64-unknown-linux-gnu':
                        self.assertEqual(runtime.component_info(directory), info)
                    else:
                        with self.assertRaises(ValueError):
                            runtime.component_info(directory)
        directory, info = self.fixture()
        info['data_epoch'] = 2
        self.write(directory, info)
        self.assertEqual(runtime.component_info(directory), info)
        with self.assertRaises(ValueError):
            release_archive.verify_component(directory, 'core')
        # runtime.guard checks against the existing data directory, not a checkout constant.

    def test_each_consumer_hashes_real_files_and_rejects_extra_files(self):
        for app in identity.COMPONENTS:
            directory, info = self.fixture(app)
            with self.subTest(app=app):
                with patch.object(release_archive, 'digest', wraps=release_archive.digest) as digest:
                    self.assertEqual(release_archive.verify_component(directory, app), info)
                    self.assertEqual(digest.call_count, 1)
                with patch.object(runtime.hashlib, 'sha256', wraps=hashlib.sha256) as digest:
                    self.assertEqual(runtime.component_info(directory), info)
                    self.assertEqual(digest.call_count, 1)
                payload = directory / next(iter(info['files']))
                payload.write_bytes(b'changed')
                for check in (lambda: release_archive.verify_component(directory, app),
                              lambda: runtime.component_info(directory)):
                    with self.assertRaises(ValueError):
                        check()
                payload.write_bytes(b'component fixture')
                (directory / 'extra').write_bytes(b'extra')
                for check in (lambda: release_archive.verify_component(directory, app),
                              lambda: runtime.component_info(directory)):
                    with self.assertRaises(ValueError):
                        check()
                (directory / 'extra').unlink()
                (directory / 'link').symlink_to(payload)
                for check in (lambda: release_archive.verify_component(directory, app),
                              lambda: runtime.component_info(directory)):
                    with self.assertRaises(ValueError):
                        check()

    def test_layout_and_every_component_pin_stay_bound(self):
        for app in identity.COMPONENTS:
            directory, info = self.fixture(app)
            for key in ('tag', 'version', 'source_commit', 'channel', 'target') + (('data_epoch',) if app == 'core' else ()):
                with self.subTest(app=app, pin=key), self.assertRaises(ValueError):
                    release_archive.verify_component(directory, app, {key: 'different'})
            prefix = '' if app == 'core' else ('web/' if app == 'app' else 'site/')
            info['files'] = {prefix + 'wrong-layout': 'a' * 64} if app == 'core' else {'wrong/index.html': 'a' * 64}
            self.write(directory, info)
            for check in (lambda: release_archive.verify_component(directory, app),
                          lambda: runtime.component_info(directory, verify=False)):
                with self.assertRaises(ValueError):
                    check()

    def test_release_manifest_still_authenticates_asset_and_inner_identity(self):
        for app in ('app', 'www'):
            directory, info = self.fixture(app)
            output = self.root / ('release-' + app)
            output.mkdir()
            archive = output / f'vibeface-{app}-0.0.1.tar.gz'
            own = {key: info[key] for key in ('app', 'version', 'tag', 'source_commit', 'channel')}
            with patch.object(release_context, 'identity', return_value=own), \
                    patch.object(release_context, 'dependencies', return_value={}):
                release_archive.public_archive(directory, archive)
                release_inventory.manifest(app, output)
                self.assertEqual(release_inventory.verify_release(app, output)['source_commit'], 'a' * 40)
                archive.write_bytes(archive.read_bytes() + b'tampered asset')
                with self.assertRaises(ValueError):
                    release_inventory.verify_release(app, output)
                for change in ({'source_commit': 'b' * 40}, {'version': '0.0.2', 'tag': app + '-v0.0.2'}):
                    # Even newly sealed asset bytes must match the outer release's
                    # exact component pin, not only their own valid inner format.
                    self.write(directory, dict(info, **change))
                    release_archive.public_archive(directory, archive)
                    release_inventory.manifest(app, output)
                    with self.assertRaises(ValueError):
                        release_inventory.verify_release(app, output)

    def test_isolated_image_context_and_import_from_unrelated_cwd(self):
        tree = ast.parse((SCRIPTS / 'deploy-run.py').read_text())
        build = next(node for node in tree.body if isinstance(node, ast.FunctionDef) and node.name == 'build')
        loop = next(node for node in build.body if isinstance(node, ast.For))
        copies = [node for node in loop.body if isinstance(node, ast.Expr) and isinstance(node.value, ast.Call)
                  and ast.unparse(node.value.func) == 'shutil.copy2']
        self.assertEqual(len(copies), 2)
        self.assertEqual([ast.unparse(node.value.args[0]) for node in copies],
                         ["ROOT / 'deploy/runtime.py'", "ROOT / 'scripts/release_component_identity.py'"])
        for app in identity.COMPONENTS:
            directory, info = self.fixture(app)
            context = self.root / ('context-' + app)
            context.mkdir()
            # Execute only the two inspected, unconditional local copy statements,
            # never the build function or any release/deploy command.
            exec(compile(ast.Module(body=copies, type_ignores=[]), '<context-copies>', 'exec'),
                 {'ROOT': ROOT, 'context': context, 'shutil': shutil})
            self.assertEqual((context / 'release_component_identity.py').read_bytes(),
                             (SCRIPTS / 'release_component_identity.py').read_bytes())
            dockerfile = (ROOT / 'deploy' / ('core.Dockerfile' if app == 'core' else 'static.Dockerfile')).read_text()
            self.assertTrue(any(line.startswith('COPY ') and 'runtime.py release_component_identity.py ' in line
                                for line in dockerfile.splitlines()))
            shutil.copy2(directory / 'COMPONENT-INFO.json', context / 'COMPONENT-INFO.json')
            if app == 'core':
                shutil.copy2(directory / 'vibeface-core', context / 'vibeface-core')
                (context / 'public-bootstrap.json').write_text('{}')
            image_runtime = load_runtime(context / 'runtime.py', 'image_runtime_' + app)
            with patch.object(image_runtime, 'ROOT', context):
                if app == 'core':
                    self.assertEqual(image_runtime.image_info(), info)
                else:
                    content = directory / ('web' if app == 'app' else 'site')
                    with patch.object(image_runtime, 'Path', return_value=content):
                        self.assertEqual(image_runtime.image_info(), info)
                        (content / 'index.html').write_bytes(b'tampered image')
                        with self.assertRaises(ValueError):
                            image_runtime.image_info()
            hostile = self.root / ('cwd-' + app)
            hostile.mkdir()
            (hostile / 'release_component_identity.py').write_text('raise AssertionError("cwd module imported")')
            code = ('import runpy; from pathlib import Path; scope=runpy.run_path(' + repr(str(context / 'runtime.py')) + '); '
                    'assert scope["component_identity"].version("0.0.1-rc.1")=="rc"; '
                    'assert not list(Path(' + repr(str(context)) + ').rglob("*.pyc"))')
            if app == 'core':
                code += ('; scope["image_info"].__globals__["ROOT"]=Path(' + repr(str(context)) + '); '
                         'assert scope["image_info"]()==' + repr(info))
            result = subprocess.run([sys.executable, '-I', '-c', code], cwd=hostile,
                                    env={'PATH': os.defpath}, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)

    def test_pure_module_has_only_standard_library_format_dependencies(self):
        tree = ast.parse((SCRIPTS / 'release_component_identity.py').read_text())
        imports = [node for node in ast.walk(tree) if isinstance(node, (ast.Import, ast.ImportFrom))]
        self.assertTrue(all(isinstance(node, ast.Import) for node in imports))
        self.assertEqual({alias.name for node in imports for alias in node.names}, {'datetime', 're'})
        self.assertIs(release_format.version, identity.version)
        self.assertIs(release_format.parse_tag, identity.parse_tag)
        self.assertIs(release_format.validate_component, identity.validate_component)

    def test_pure_module_exec_and_validation_have_no_external_access(self):
        compiled = compile((SCRIPTS / 'release_component_identity.py').read_text(), '<detached-identity>', 'exec')
        _, info = self.fixture()
        bytecode_policy = sys.dont_write_bytecode
        # Prime the standard-library date parser before forbidding external access.
        identity.version('0.0.1-hourly.2026111700')

        def forbidden(*args, **kwargs):
            raise AssertionError('pure component format attempted external access')

        class NoEnvironment(dict):
            __getitem__ = get = __iter__ = forbidden

        with patch.object(builtins, 'open', forbidden), patch.object(Path, 'open', forbidden), \
                patch.object(subprocess, 'Popen', forbidden), patch.object(subprocess, 'run', forbidden), \
                patch.object(subprocess, 'check_output', forbidden), patch.object(socket, 'socket', forbidden), \
                patch.object(os, 'getenv', forbidden), patch.object(os, 'environ', NoEnvironment()):
            namespace = {'__name__': 'detached_component_identity'}
            exec(compiled, namespace)
            self.assertEqual(namespace['validate_component'](info), info)
            self.assertEqual(namespace['version']('0.0.1-hourly.2026111700'), 'hourly')
            self.assertEqual(namespace['parse_tag']('app-v0.0.1-rc.1'), ('app', '0.0.1-rc.1', 'rc'))
            self.assertEqual(sys.dont_write_bytecode, bytecode_policy)


if __name__ == '__main__':
    unittest.main()
