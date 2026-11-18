#!/usr/bin/env python3
"""Exact source-generation and eligibility tests in disposable Git repositories."""
import sys
sys.dont_write_bytecode = True
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
import base64
import ast
import runpy
import textwrap
import json
import os
import subprocess
import unittest
from unittest.mock import patch
import release_context
import release_format
import release_source
import release_publisher
import hourly
from tests.release.support import Fixture, fixture_repository, pin, MODULES, SOURCE


class SourceContracts(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temporary, cls.root, cls.environment = fixture_repository()
        cls.patches = [patch.object(release_context, 'ROOT', cls.root), patch.dict(os.environ,cls.environment,clear=True)]
        for p in cls.patches:p.start()
        cls.base=release_context.git('rev-parse','HEAD')
        release_context.git('update-ref','refs/remotes/origin/main',cls.base)

    @classmethod
    def tearDownClass(cls):
        for p in reversed(cls.patches):p.stop()
        cls.temporary.cleanup()

    def commit(self,tree,message='chore(release): core hourly 2026111409',parents=None,author=None):
        env=dict(os.environ,GIT_AUTHOR_EMAIL=author or release_source.BOT,GIT_COMMITTER_EMAIL=author or release_source.BOT)
        args=['commit-tree',tree]
        for parent in parents or [self.base]:args+=['-p',parent]
        return release_context.git(*args,'-m',message,env=env)

    def test_generated_bytes_match_fixed_baseline_all_six_apps(self):
        for app in ('core','app','cli','desktop','mobile','www'):
            pins={d:pin(d) for d in release_format.DEPENDENCIES[app]}
            value,content=release_source.generated(app,self.base,'2026111409',pins)
            self.assertEqual(value,'0.0.1-hourly.2026111409')
            self.assertEqual(set(content),{release_source.version_path(app)} | ({'Cargo.lock'} if app in ('core','cli','desktop') else set()) | ({'releases/'+app+'.json'} if pins else set()))
            with Fixture(True) as baseline:
                # Only the scratch repository is writable by Git in this subprocess.
                code='import hourly,json,base64; from pathlib import Path; hourly.release.ROOT=Path('+repr(str(self.root))+'); value,content=hourly.generated('+repr(app)+','+repr(self.base)+',"2026111409",'+repr(pins)+'); print(json.dumps([value,{k:base64.b64encode(v).decode() for k,v in content.items()}],sort_keys=True))'
                env=dict(self.environment,PYTHONPATH=str(baseline.scripts))
                result=subprocess.run([sys.executable,'-B','-c',code],cwd=self.root,env=env,capture_output=True,text=True,check=True)
                expected=json.loads(result.stdout)
                self.assertEqual([value,{k:base64.b64encode(v).decode() for k,v in content.items()}],expected)
            if app=='mobile':
                expo=json.loads(content['apps/mobile/app.json'])['expo']
                self.assertEqual(expo['android']['versionCode'],26111409)
                self.assertEqual(expo['ios']['buildNumber'],'2611.14.9')
            elif app in ('core','cli','desktop'):
                before=release_context.blob(self.base,'Cargo.lock').split(b'[[package]]')
                after=content['Cargo.lock'].split(b'[[package]]')
                changed=[a for a,b in zip(before,after) if a!=b]
                self.assertEqual(len(changed),1)
                self.assertIn(('name = "'+{'core':'vibeface-core','cli':'vibeface','desktop':'vibeface-desktop'}[app]+'"').encode(),changed[0])

    def test_exact_tree_author_single_parent_message_and_extra_change(self):
        tree=release_source.expected_tree('core',self.base,'2026111409')
        source=self.commit(tree)
        self.assertEqual(release_source.verify(source,'core'),(self.base,'2026111409'))
        self.assertEqual(release_source.validate_source(source,'core'),{'ci_source_commit':self.base})
        self.assertEqual(release_source.validate_source(self.base,'core'),{'ci_source_commit':self.base})
        cases=[self.commit(tree,author='other@example.invalid'),self.commit(tree,message='wrong message'),
               self.commit(tree,parents=[self.base,self.commit(tree,message='other parent')])]
        with __import__('tempfile').TemporaryDirectory() as tmp:
            env=dict(os.environ,GIT_INDEX_FILE=str(Path(tmp)/'index'))
            release_context.git('read-tree',tree,env=env)
            oid=release_context.git('hash-object','-w','--stdin',data=b'extra bytes')
            release_context.git('update-index','--add','--cacheinfo','100644',oid,'extra.txt',env=env)
            cases.append(self.commit(release_context.git('write-tree',env=env)))
        for bad in cases:
            with self.subTest(source=bad),self.assertRaises(ValueError):release_source.verify(bad,'core')
        release_context.git('update-ref','refs/remotes/origin/main',cases[-1])
        # A valid hourly child whose parent is not on main is refused.
        unrelated=self.commit(tree,parents=[self.base],message='unrelated')
        nexttree=release_source.expected_tree('core',unrelated,'2026111409')
        detached=self.commit(nexttree,parents=[unrelated])
        with self.assertRaisesRegex(ValueError,'parent must be on main'):release_source.validate_source(detached,'core')
        release_context.git('update-ref','refs/remotes/origin/main',self.base)

    def test_exact_ci_every_eligibility_field(self):
        source='a'*40
        good=dict(head_sha=source,head_branch='main',path='.github/workflows/ci.yml',event='push',
            repository={'full_name':'leaperone/vibeface'},head_repository={'full_name':'leaperone/vibeface'},status='completed',conclusion='success')
        with patch('release_github.ci_runs',return_value=[{'workflow_runs':[good]}]) as call:
            self.assertEqual(release_publisher.require_ci({'ci_source_commit':source}),good)
            call.assert_called_once_with(source)
        for key,value in dict(head_sha='b'*40,head_branch='feature',path='.github/workflows/other.yml',event='workflow_dispatch',repository={'full_name':'other/repo'},head_repository={'full_name':'other/repo'},status='in_progress',conclusion='failure').items():
            with self.subTest(key=key),patch('release_github.ci_runs',return_value=[{'workflow_runs':[dict(good,**{key:value})]}]),self.assertRaises(ValueError):
                release_publisher.require_ci({'ci_source_commit':source})

    def test_current_hourly_selftest_and_inputs_in_fixture(self):
        result = subprocess.run([sys.executable, str(self.root / 'scripts/hourly.py'), '--self-test'],
            cwd=self.root, env=self.environment, capture_output=True, text=True)
        self.assertEqual((result.returncode, result.stdout, result.stderr), (0,
            'Independent Hourly self-test passed: selected version only, exact pins and per-train build numbers\n', ''))
        self.assertEqual(release_context.git('status','--porcelain'),'')
        self.assertEqual(list(self.root.rglob('__pycache__')), [])

    def test_hourly_reconcile_one_write_and_bounded_reads(self):
        for outcome in ('delayed', 'unknown'):
            write = __import__('unittest.mock', fromlist=['Mock']).Mock(side_effect=subprocess.CalledProcessError(7, ['fixture-write']))
            reads = [None, None, {'id': 1}] if outcome == 'delayed' else [None] * 18
            read = __import__('unittest.mock', fromlist=['Mock']).Mock(side_effect=reads)
            with patch.object(hourly.time, 'sleep') as pause:
                if outcome == 'delayed':
                    self.assertEqual(hourly.reconcile(write, read, 'fixture'), {'id': 1})
                    self.assertEqual(read.call_count, 3)
                    self.assertEqual(pause.call_count, 2)
                else:
                    with self.assertRaisesRegex(ValueError, 'reconcile before retrying'):
                        hourly.reconcile(write, read, 'fixture')
                    self.assertEqual(read.call_count, 18)
                    self.assertEqual(pause.call_count, 18)
                write.assert_called_once_with()
                for call in pause.call_args_list:
                    self.assertEqual(call.args, (5,))

    def test_production_graph_and_owner_uniqueness(self):
        expected = {
            'release_component_identity': set(), 'release_format': {'release_component_identity'},
            'release_context': {'release_format'},
            'release_source': {'release_format', 'release_context'},
            'release_archive': {'release_format', 'release_context'},
            'release_inventory': {'release_format', 'release_context', 'release_archive'},
            'release_assembly': {'release_format', 'release_context', 'release_archive', 'release_inventory'},
            'release_github': {'release_format', 'release_context', 'release_archive'},
            'release_publisher': {'release_format', 'release_context', 'release_source', 'release_archive', 'release_inventory', 'release_github'}}
        functions = {}
        for module in MODULES:
            tree = ast.parse((SOURCE / (module + '.py')).read_text())
            edges = set()
            for node in ast.walk(tree):
                if isinstance(node, ast.Import):
                    for alias in node.names:
                        if alias.name.startswith('release_'):
                            edges.add(alias.name)
                        self.assertNotIn(alias.name, ('release', 'hourly', 'tests'))
                if isinstance(node, ast.ImportFrom):
                    if module == 'release_format' and node.module == 'release_component_identity':
                        edges.add(node.module)
                    else:
                        self.assertFalse((node.module or '').startswith(('release', 'hourly', 'tests')))
                if isinstance(node, ast.Attribute) and node.attr == 'dont_write_bytecode':
                    self.fail('library changes global bytecode policy')
            self.assertEqual(edges, expected[module])
            for node in tree.body:
                if isinstance(node, ast.FunctionDef):
                    self.assertNotIn(node.name, functions)
                    functions[node.name] = module
        for name in ('git', 'blob', 'run', 'identity', 'contracts', 'dependencies'):
            self.assertEqual(functions[name], 'release_context')
        for name in ('generated', 'expected_tree', 'verify', 'validate_source'):
            self.assertEqual(functions[name], 'release_source')
        self.assertEqual(functions['manifest'], 'release_inventory')
        self.assertEqual(functions['public_archive'], 'release_archive')
        hourly_tree = ast.parse((SOURCE / 'hourly.py').read_text())
        self.assertFalse({n.name for n in hourly_tree.body if isinstance(n, ast.FunctionDef)} &
            {'git','blob','hour_of','build_numbers','version_path','hourly_version','generated','expected_tree','verify','source_context','validate_source'})
        self.assertEqual([n.name for n in ast.parse((SOURCE/'release.py').read_text()).body if isinstance(n,ast.FunctionDef)], ['main'])

    def test_format_detached_no_io_process_environment_or_global_side_effect(self):
        # Compile before installing the guards. No project helper or fixture-root
        # lookup is available to the format module during its execution.
        source = (SOURCE / 'release_format.py').read_text()
        identity_source = (SOURCE / 'release_component_identity.py').read_text()
        code = textwrap.dedent("""
            import builtins, datetime, pathlib, re, os, socket, subprocess, sys, types
            from unittest.mock import patch
            source = SOURCE_TEXT
            compiled = compile(source, 'detached-release-format', 'exec')
            identity_compiled = compile(IDENTITY_SOURCE_TEXT, 'detached-component-identity', 'exec')
            def forbidden(*a, **kw): raise AssertionError('format attempted external access')
            class NoEnvironment(dict):
                def __getitem__(self, key): forbidden()
                def get(self, *args): forbidden()
                def __iter__(self): forbidden()
            sys.dont_write_bytecode = False
            with patch.object(builtins, 'open', forbidden), patch.object(pathlib.Path, 'open', forbidden), \
                 patch.object(subprocess, 'Popen', forbidden), patch.object(subprocess, 'run', forbidden), \
                 patch.object(subprocess, 'check_output', forbidden), patch.object(socket, 'socket', forbidden), \
                 patch.object(os, 'getenv', forbidden), patch.object(os, 'environ', NoEnvironment()):
                # Supply the leaf from bytes in memory; its import cannot consult
                # a checkout, cwd, environment, or a filesystem loader.
                leaf = types.ModuleType('release_component_identity')
                exec(identity_compiled, leaf.__dict__)
                sys.modules['release_component_identity'] = leaf
                namespace = {'__name__': 'detached_release_format'}
                exec(compiled, namespace)
                assert sys.dont_write_bytecode is False
                assert namespace['parse_tag']('app-v1.2.3') == ('app', '1.2.3', 'stable')
                build = dict(version='1.2.3', target='x86_64-unknown-linux-gnu', source_commit='a'*40,
                    core_version='2.3.4', core_source_commit='b'*40, app_version='3.4.5-rc.2', app_source_commit='c'*40,
                    data_epoch=1, update_protocol=1)
                assert namespace['verify_build_info'](build, {'data_epoch':1,'update_protocol':1}) == build
            print('detached format has no external access')
        """).replace('IDENTITY_SOURCE_TEXT', repr(identity_source)).replace('SOURCE_TEXT', repr(source))
        result = subprocess.run([sys.executable, '-B', '-c', code], cwd=self.root,
            env=self.environment, capture_output=True, text=True)
        self.assertEqual((result.returncode,result.stdout,result.stderr),
                         (0,'detached format has no external access\n',''))

    def test_ci_selects_every_new_production_and_test_path(self):
        select = runpy.run_path(str(SOURCE / 'ci.py'))['make_plan']
        for module in MODULES:
            self.assertTrue(select(['scripts/' + module + '.py'])['full'])
        for path in ('scripts/tests/release/test_cli.py', 'scripts/tests/release/test_publisher.py',
                     'scripts/tests/release/test_source.py', 'scripts/tests/release/support.py'):
            self.assertTrue(select([path])['full'])

    def test_invalid_hour_and_dependency_contracts(self):
        for batch in ('2026023000','2026111424','20261114','invalid'):
            with self.assertRaises(ValueError):release_source.hour_of(batch)
        with self.assertRaises(ValueError):release_source.generated('cli',self.base,'2026111409',{})
        with self.assertRaises(ValueError):release_source.validate_source('not-a-sha','core')


if __name__=='__main__':
    unittest.main()
