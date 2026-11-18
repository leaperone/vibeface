#!/usr/bin/env python3
"""Real release CLI compatibility, literal bytes and minimal-copy execution."""
import sys
sys.dont_write_bytecode = True
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
import ast
import hashlib
import io
import json
import os
import plistlib
import shutil
import subprocess
import zipfile
from unittest.mock import patch
import release_context
import release_archive
import release_inventory
import tarfile
import unittest
from tests.release.support import Fixture, SOURCE, TARGETS, CLI, VERSIONS, CONTRACTS, pin, write_json

IDENTITY = '{"app": "cli", "version": "0.0.1-hourly.2026111409", "tag": "cli-v0.0.1-hourly.2026111409", "source_commit": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "channel": "hourly"}\n'
SELF_TEST = 'Independent release self-test passed: six tags, independent pins, sealed bytes and tampering rejection\n'


def tree_bytes(directory):
    return {p.relative_to(directory).as_posix(): p.read_bytes() for p in directory.rglob('*') if p.is_file()}


def sidecars(test, output):
    for sidecar in output.glob('*.sha256'):
        payload = sidecar.with_suffix('')
        test.assertEqual(sidecar.read_bytes(), (hashlib.sha256(payload.read_bytes()).hexdigest() + '  ' + payload.name + '\n').encode())


def component_inputs(fixture, target=TARGETS[2]):
    fixture.change(head='c' * 40, on_main=['a' * 40, 'b' * 40, 'c' * 40])
    app = fixture.component('app', output='app-' + target)
    fixture.change(head='b' * 40)
    core = fixture.component('core', target=target)
    fixture.change(head='a' * 40)
    binary = fixture.native('cli', target=target)
    staged = fixture.root / ('staged-' + target)
    fixture.cli('stage-native', '--target', target, '--input', binary.parent, '--output', staged)
    return staged, next(app.glob('*.tar.gz')), next(core.glob('*.tar.gz'))


def desktop_fixture(fixture, target):
    out = fixture.root / ('desktop-' + target)
    out.mkdir()
    value = VERSIONS['desktop']
    base = f'vibeface-desktop-{value}-{target}'
    extension = 'dmg' if target == TARGETS[0] else 'exe' if target == TARGETS[3] else 'AppImage'
    names = [base + '.' + extension, base + '.zip']
    for name in names:
        path = out / name
        path.write_bytes(b'desktop payload\n' + name.encode())
        path.with_name(name + '.sha256').write_text(hashlib.sha256(path.read_bytes()).hexdigest() + '  ' + name + '\n')
    signing = 'platform-checksums' if 'linux' in target else 'unsigned-development-preview'
    receipt = dict(schema=2, app='desktop', target=target, version=value, tag='desktop-v' + value,
        channel='hourly', source_commit='a' * 40, ui='gpui', gpui_kit='0.9.1', gpui='0.3.12', signed=False,
        signing=signing, runtime=dict(version=CLI, source_commit='a' * 40,
            archive=f'vibeface-{CLI}-{target}.tar.gz', sha256='f' * 64),
        artifacts={name: hashlib.sha256((out / name).read_bytes()).hexdigest() for name in names})
    write_json(out / (base + '.provenance.json'), receipt)
    return out


class CliContracts(unittest.TestCase):
    def test_plan_literal_and_append_order(self):
        for baseline in (True, False):
            with self.subTest(baseline=baseline), Fixture(baseline) as f:
                self.assertEqual(f.cli('plan', '--app', 'cli').stdout, IDENTITY)
                output = f.root / 'github-output'
                output.write_text('sentinel\n')
                result = f.cli('plan', '--app', 'cli', '--github-output', output)
                self.assertEqual((result.stdout, result.stderr), (IDENTITY, ''))
                expected = 'sentinel\napp=cli\nversion=' + CLI + '\ntag=cli-v' + CLI + '\nsource_commit=' + 'a' * 40 + '\nchannel=hourly\n'
                self.assertEqual(output.read_text(), expected)
                for option in ('--require-main', '--publish'):
                    result = f.cli('plan', '--app', 'cli', option, '--github-output', output)
                    value = json.loads(IDENTITY)
                    value['ci_source_commit'] = 'a' * 40
                    self.assertEqual((result.stdout, result.stderr), (json.dumps(value) + '\n', ''))
                    self.assertTrue(output.read_text().endswith(expected[len('sentinel\n'):] + 'ci_source_commit=' + 'a' * 40 + '\n'))
                self.assertFalse(any(e['args'][:2] in (['tag', '-a'], ['push', 'origin']) for e in f.state()['events']))

    def test_cli_parser_exact_shape(self):
        with Fixture(True) as baseline:
            def statements(path):
                tree = ast.parse(path.read_text())
                main = next(n for n in tree.body if isinstance(n, ast.FunctionDef) and n.name == 'main')
                # Qualified constants differ; all parser operations, literal flags,
                # defaults and order are identical after removing owner prefixes.
                return [ast.dump(n, include_attributes=False).replace("Attribute(value=Name(id='release_format', ctx=Load()), attr=", 'Name(id=').replace(", ctx=Load()), ctx=Load())", ', ctx=Load())')
                        for n in main.body if not isinstance(n, ast.If)]
            # Compare source parser statements through parse_args, preserving all defaults.
            def parser_source(path):
                text = path.read_text()
                start = text.index('    parser = argparse.ArgumentParser')
                end = text.index('    if args.command')
                return text[start:end].replace('release_format.', '')
            self.assertEqual(parser_source(SOURCE / 'release.py'), parser_source(baseline.scripts / 'release.py'))
            self.assertEqual({n.args[0].value for n in ast.walk(ast.parse((SOURCE / 'release.py').read_text()))
                if isinstance(n, ast.Call) and isinstance(n.func, ast.Attribute) and n.func.attr == 'add_parser' and n.args and isinstance(n.args[0], ast.Constant)},
                {'plan','component','stage-native','compose','seal-bundle','manifest','collect','dependency','verify-desktop','mobile-provenance','self-test'})

    def test_argparse_and_runtime_errors(self):
        with Fixture() as f:
            for argv in ([], ['unknown'], ['plan'], ['plan', '--app', 'invalid'], ['component','app']):
                self.assertEqual(f.cli(*argv, code=2).stdout, '')
            for tag in ('core-v' + CLI, 'cli-v1.0.0-nightly.1', 'cli-v1.0.0-hourly.2026111424', 'cli-v1.0.0-hourly.2026023000'):
                self.assertEqual(f.cli('plan','--app','cli','--tag',tag,code=1).stdout, '')
            for value in ('1.0.0-hourly.2026111424', '1.0.0-hourly.2026023000', 'invalid'):
                (f.root/'apps/cli/Cargo.toml').write_text('version = ' + json.dumps(value))
                self.assertEqual(f.cli('plan','--app','cli',code=1).stdout, '')

    def test_components_real_archives_and_baseline_bytes(self):
        observed = []
        for baseline in (True, False):
            with Fixture(baseline) as f:
                outputs = {}
                for app in ('app','www','core'):
                    output = f.component(app, TARGETS[2] if app == 'core' else None)
                    sidecars(self, output)
                    outputs[app] = tree_bytes(output)
                    manifest = json.loads((output/'release.json').read_text())
                    self.assertEqual(len(manifest['assets']),2)
                    self.assertNotIn('release.json', [a['name'] for a in manifest['assets']])
                    gzip_header = next(output.glob('*.tar.gz')).read_bytes()[:10]
                    self.assertEqual(gzip_header[4:8], b'\0' * 4)
                    self.assertFalse(gzip_header[3] & 8)
                    with tarfile.open(next(output.glob('*.tar.gz'))) as archive:
                        names = archive.getnames()
                        root = names[0]
                        info = json.load(archive.extractfile(root+'/COMPONENT-INFO.json'))
                        self.assertEqual(info['app'],app)
                        self.assertEqual(info['source_commit'],'a'*40)
                        self.assertEqual(info['version'],VERSIONS[app])
                        for entry in archive.getmembers():
                            self.assertEqual((entry.uid,entry.gid,entry.uname,entry.gname,entry.mtime),(0,0,'','',0))
                            self.assertEqual(entry.mode,0o755 if entry.isdir() or entry.name.endswith('vibeface-core') else 0o644)
                        if app=='core': self.assertEqual(info['data_epoch'],1)
                        else:self.assertEqual(archive.extractfile(root+('/web/' if app=='app' else '/site/')+'index.html').read(),b'fixture app\n')
                wrong=f.native('core','9.9.9',directory='wrong')
                f.cli('component','core','--target',TARGETS[2],'--input',wrong,'--output',f.root/'bad',code=1)
                self.assertFalse(list((f.root/'bad').glob('*.tar.gz')))
                observed.append(outputs)
        self.assertEqual(*observed)

    def test_stage_compose_seal_manifest_real_bytes(self):
        observed=[]
        for baseline in (True,False):
            with Fixture(baseline) as f:
                staged,app,core=component_inputs(f)
                native=json.loads((staged/'NATIVE-INFO.json').read_text())
                self.assertEqual({k:native[k] for k in ('schema','target','version','source_commit','data_epoch')},
                    dict(schema=1,target=TARGETS[2],version=CLI,source_commit='a'*40,data_epoch=1))
                self.assertEqual(set(native['files']),{'vibeface'})
                f.cli('stage-native','--target',TARGETS[2],'--input',f.root/'native-input','--output',staged,code=1)
                f.change(dirty=' M source')
                f.cli('stage-native','--target',TARGETS[2],'--input',f.root/'native-input','--output',f.root/'dirty-stage',code=1)
                f.change(dirty='')
                out=f.root/'composed'
                argv=['compose','--target',TARGETS[2],'--native-dir',staged,'--app-archive',app,'--core-archive',core,'--output',out]
                result=f.cli(*argv)
                self.assertEqual((result.stdout,result.stderr),('',''))
                sidecars(self,out)
                with tarfile.open(next(out.glob('*.tar.gz'))) as archive:
                    root=archive.getnames()[0]
                    build=json.load(archive.extractfile(root+'/BUILD-INFO.json'))
                    self.assertEqual(build,dict(version=CLI,target=TARGETS[2],source_commit='a'*40,
                        core_version='2.3.4',core_source_commit='b'*40,app_version='3.4.5-rc.2',app_source_commit='c'*40,**CONTRACTS))
                    self.assertEqual(hashlib.sha256(archive.extractfile(root+'/RUNTIME-REQUIREMENTS.md').read()).hexdigest(),
                        '583c6febbbb822691ef41f2210ea5cec36a9e4a18aa7cbed957e0bf38a66a0b3')
                    self.assertEqual(archive.extractfile(root+'/web/index.html').read(),b'fixture app\n')
                observed.append(tree_bytes(out))
                for dep in ('app','core'):
                    pins={'core':pin('core'),'app':pin('app')};pins[dep]['source_commit']='f'*40
                    write_json(f.root/'releases/cli.json',pins)
                    args=argv.copy();args[-1]=f.root/('bad-pin-'+dep)
                    f.cli(*args,code=1)
                package=f.root/'seal';package.mkdir();(package/'payload').write_text('fixture\n')
                f.cli('seal-bundle',package,PACKAGE_TARGET=TARGETS[2])
                first=tree_bytes(package)
                f.cli('seal-bundle',package,PACKAGE_TARGET=TARGETS[2])
                self.assertEqual(first,tree_bytes(package))
                self.assertTrue((package/'FILES.sha256').read_bytes().endswith(b'  payload\n'))
                f.cli('manifest','www',package)
                manifest_bytes=(package/'release.json').read_bytes()
                self.assertTrue(manifest_bytes.endswith(b'\n'))
                self.assertEqual(manifest_bytes,(json.dumps(json.loads(manifest_bytes),indent=2,sort_keys=True)+'\n').encode())
                f.cli('manifest','www',package)
                self.assertEqual((package/'release.json').read_bytes(),manifest_bytes)
        self.assertEqual(*observed)

    def test_collect_complete_core_and_negative_sets(self):
        with Fixture() as f:
            input_root=f.root/'producers'
            for target in TARGETS:f.component('core',target,output='producers/'+target)
            output=f.root/'collection'
            self.assertEqual(f.cli('collect','--app','core','--input-root',input_root,'--output',output).stdout,'')
            self.assertEqual(len(list(output.iterdir())),9)
            sidecars(self,output)
            # Core downloads must select one exact native target from a complete release.
            info = f.remote(output)
            marker_sha = hashlib.sha256((output / 'release.json').read_bytes()).hexdigest()
            write_json(f.root / 'releases/cli.json', {'core': pin('core', source='a'*40, sha=marker_sha), 'app': pin('app')})
            for target in TARGETS:
                destination = f.root / ('dependency-' + target)
                f.cli('dependency', '--app', 'cli', '--dependency', 'core', '--target', target, '--output', destination)
                expected_name = 'vibeface-core-2.3.4-' + target + '.tar.gz'
                self.assertEqual({p.name for p in destination.iterdir()}, {expected_name, expected_name + '.sha256'})
            for extra in ([], ['--target', 'invalid']):
                f.cli('dependency', '--app', 'cli', '--dependency', 'core', '--output', f.root / 'bad-target', *extra, code=1)
            f.cli('collect','--app','core','--input-root',input_root,'--output',output,code=1)
            import shutil
            shutil.copytree(input_root/TARGETS[0],input_root/'duplicate')
            bad=f.root/'duplicate-out'
            f.cli('collect','--app','core','--input-root',input_root,'--output',bad,code=1)
            self.assertTrue(bad.exists())
            shutil.rmtree(input_root/'duplicate')
            extra = input_root / TARGETS[0] / 'extra.txt'
            extra.write_text('foreign producer file')
            f.cli('collect','--app','core','--input-root',input_root,'--output',f.root/'extra',code=1)
            extra.unlink()
            shutil.rmtree(input_root/TARGETS[0])
            f.cli('collect','--app','core','--input-root',input_root,'--output',f.root/'missing',code=1)

    def test_dependency_verify_public_and_download_integrity(self):
        for baseline in (True,False):
            with Fixture(baseline) as f:
                f.change(head='c'*40,on_main=['a'*40,'c'*40])
                out=f.component('app');info=f.remote(out)
                marker=hashlib.sha256((out/'release.json').read_bytes()).hexdigest()
                write_json(f.root/'releases/cli.json',{'core':pin('core'),'app':pin('app',sha=marker)})
                f.change(head='a'*40)
                dest=f.root/'dep'
                self.assertEqual(f.cli('dependency','--app','cli','--dependency','app','--output',dest).stdout,'')
                self.assertEqual(len(list(dest.iterdir())),2)
                payload=next(dest.glob('*.tar.gz'));payload.write_bytes(b'bad')
                before=len([e for e in f.state()['events'] if e['args'][:2]==['release','download']])
                f.cli('dependency','--app','cli','--dependency','app','--output',dest,code=1)
                self.assertEqual(before,len([e for e in f.state()['events'] if e['args'][:2]==['release','download']]))
                f.change(head='c'*40)
                public=f.root/'public'
                f.cli('verify-public','app',public)
                self.assertEqual(tree_bytes(public),tree_bytes(out))

    def test_desktop_all_targets_runtime_and_signing_negatives(self):
        for baseline in (True,False):
            with Fixture(baseline) as f:
                for target in TARGETS:
                    out=desktop_fixture(f,target)
                    f.cli('verify-desktop','--target',target,out)
                    receipt=next(out.glob('*.provenance.json'));info=json.loads(receipt.read_text())
                    for key,value in [('schema',1),('gpui','0.0.0'),('signed',True),('signing','platform-checksums' if 'linux' not in target else 'unsigned-development-preview')]:
                        write_json(receipt,dict(info,**{key:value}));f.cli('verify-desktop','--target',target,out,code=1)
                    write_json(receipt,info)
                    runtime=f.root/'runtime';runtime.write_bytes(b'bad runtime')
                    f.cli('verify-desktop','--target',target,'--runtime-archive',runtime,out,code=1)

    def test_stable_desktop_requires_platform_signature(self):
        for target, signing in ((TARGETS[0], 'developer-id-notarized'), (TARGETS[3], 'authenticode')):
            with Fixture() as f:
                original = desktop_fixture(f, target)
                stable = f.root / 'stable-desktop'
                stable.mkdir()
                receipt = json.loads(next(original.glob('*.provenance.json')).read_text())
                artifacts = {}
                for source in original.iterdir():
                    if source.name.endswith('.provenance.json'):
                        continue
                    destination = stable / source.name.replace(VERSIONS['desktop'], '4.5.6')
                    if destination.name.endswith('.sha256'):
                        continue
                    shutil.copy2(source, destination)
                    sha = hashlib.sha256(destination.read_bytes()).hexdigest()
                    artifacts[destination.name] = sha
                    destination.with_name(destination.name + '.sha256').write_text(sha + '  ' + destination.name + '\n')
                receipt.update(version='4.5.6', tag='desktop-v4.5.6', channel='stable', artifacts=artifacts)
                path = stable / ('vibeface-desktop-4.5.6-' + target + '.provenance.json')
                write_json(path, receipt)
                (f.root / 'apps/desktop/Cargo.toml').write_text('version = "4.5.6"\n')
                f.cli('verify-desktop', '--target', target, stable, code=1)
                receipt.update(signed=True, signing=signing)
                write_json(path, receipt)
                f.cli('verify-desktop', '--target', target, stable)

    def test_mobile_provenance_and_identity_negatives(self):
        for baseline in (True,False):
            with Fixture(baseline) as f:
                apk=f.root/'fixture.apk';apk.write_bytes(b'apk fixture\n')
                self.assertEqual(f.cli('mobile-provenance',apk).stdout,'')
                self.assertEqual(json.loads(apk.with_name(apk.name+'.provenance.json').read_text()),dict(version=CLI,sourceCommit='a'*40,
                    sha256=hashlib.sha256(apk.read_bytes()).hexdigest(),applicationId='cn.leaperone.vibeface',versionCode=26111409,versionName='1.0.0'))
                for badging in ("package: name='other' versionCode='26111409' versionName='1.0.0'", "package: name='cn.leaperone.vibeface' versionCode='1' versionName='1.0.0'", "package: name='cn.leaperone.vibeface' versionCode='26111409' versionName='9.0.0'"):
                    f.change(badging=badging);f.cli('mobile-provenance',apk,code=1)

    def test_publish_and_selftest_real_entrypoints(self):
        for baseline in (True,False):
            with Fixture(baseline) as f:
                out=f.component('app')
                result=f.cli('publish','app',out)
                info=json.loads(result.stdout)
                self.assertEqual(info,dict(app='app',version='3.4.5-rc.2',tag='app-v3.4.5-rc.2',source_commit='a'*40,channel='rc',ci_source_commit='a'*40))
                self.assertEqual(result.stderr,'')
                self.assertEqual(f.cli('self-test').stdout,SELF_TEST)

    def test_real_complete_release_asset_counts_and_mobile_receipts(self):
        with Fixture() as f:
            for app in ('app', 'www'):
                self.assertEqual(len(list(f.component(app).iterdir())), 3)
            producers = f.root / 'cli-producers'
            for target in TARGETS:
                staged, app, core = component_inputs(f, target)
                f.cli('compose', '--target', target, '--native-dir', staged,
                      '--app-archive', app, '--core-archive', core,
                      '--output', producers / target)
            cli_out = f.root / 'cli-complete'
            f.cli('collect', '--app', 'cli', '--input-root', producers, '--output', cli_out)
            self.assertEqual(len(list(cli_out.iterdir())), 9)
            producer_marker = producers / TARGETS[0] / 'release.json'
            bad_pins = json.loads(producer_marker.read_text())
            bad_pins['dependencies']['core']['source_commit'] = 'f' * 40
            write_json(producer_marker, bad_pins)
            f.cli('collect', '--app', 'cli', '--input-root', producers, '--output', f.root / 'producer-pin-mismatch', code=1)

            desktop = f.root / 'desktop-complete'
            desktop.mkdir()
            for target in TARGETS:
                for source in desktop_fixture(f, target).iterdir():
                    shutil.copy2(source, desktop / source.name)
            f.cli('manifest', 'desktop', desktop)
            self.assertEqual(len(list(desktop.iterdir())), 21)
            with patch.object(release_context, 'ROOT', f.root), patch.dict(os.environ, f.env, clear=True):
                release_inventory.verify_release('desktop', desktop)
            for signed, count in ((False, 7), (True, 10)):
                mobile = f.root / ('mobile-signed' if signed else 'mobile-unsigned')
                mobile.mkdir()
                apk = mobile / ('vibeface-' + CLI + '-android.apk')
                apk.write_bytes(b'APK bytes')
                f.cli('mobile-provenance', apk)
                apk.with_name(apk.name + '.sha256').write_text(hashlib.sha256(apk.read_bytes()).hexdigest() + '  ' + apk.name + '\n')
                state = 'signed' if signed else 'unsigned'
                names = [f'vibeface-{CLI}-ios-device-{state}.xcarchive.zip']
                if signed:
                    names.append(f'vibeface-{CLI}-ios.ipa')
                for index, name in enumerate(names):
                    path = mobile / name
                    info_name = 'Payload/Vibeface.app/Info.plist' if index else 'Vibeface.xcarchive/Products/Applications/Vibeface.app/Info.plist'
                    with zipfile.ZipFile(path, 'w') as archive:
                        archive.writestr(info_name, plistlib.dumps(dict(CFBundleIdentifier='cn.leaperone.vibeface',
                            CFBundleShortVersionString='1.0.0', CFBundleVersion='2611.14.9', CFBundleSupportedPlatforms=['iPhoneOS'])))
                        if signed:
                            archive.writestr(info_name.rsplit('/', 1)[0] + '/embedded.mobileprovision', b'fixture provisioning profile')
                    sha = hashlib.sha256(path.read_bytes()).hexdigest()
                    path.with_name(name + '.sha256').write_text(sha + '  ' + name + '\n')
                    receipt = dict(schema=1, platform='ios-device', source_sha='a' * 40, release_version=CLI,
                        bundle_identifier='cn.leaperone.vibeface', bundle_short_version='1.0.0', build_number='2611.14.9',
                        artifact=name, sha256=sha, signed=signed, installable=bool(index),
                        kind='signed-ios-ipa' if index else state + '-xcarchive-inspection-only')
                    if index:
                        receipt.update(distribution_method='ad-hoc', team_id='ABCDE12345', provisioning_profile_uuid='fixture-uuid',
                                       provisioning_profile_expiration='2027-01-01T00:00:00Z', testflight_eligible=False)
                    write_json(path.with_name(name + '.provenance.json'), receipt)
                f.cli('manifest', 'mobile', mobile)
                self.assertEqual(len(list(mobile.iterdir())), count)
                with patch.object(release_context, 'ROOT', f.root), patch.dict(os.environ, f.env, clear=True):
                    release_inventory.verify_release('mobile', mobile)
                    receipt = mobile / (names[0] + '.provenance.json')
                    bad = json.loads(receipt.read_text())
                    bad['signed'] = not signed
                    write_json(receipt, bad)
                    # Refresh manifest metadata to ensure receipt semantics, rather than its hash, fail.
                    f.cli('manifest', 'mobile', mobile)
                    with self.assertRaises(ValueError):
                        release_inventory.verify_release('mobile', mobile)

    def test_unpack_rejects_links_traversal_duplicates_special_and_bounds(self):
        with Fixture() as f:
            for mode in ('traversal', 'duplicate', 'symlink', 'hardlink', 'fifo', 'multiple-roots'):
                path = f.root / (mode + '.tar.gz')
                with tarfile.open(path, 'w:gz') as archive:
                    entry = tarfile.TarInfo('root/file' if mode != 'traversal' else 'root/../escape')
                    if mode in ('symlink', 'hardlink', 'fifo'):
                        entry.type = {'symlink': tarfile.SYMTYPE, 'hardlink': tarfile.LNKTYPE, 'fifo': tarfile.FIFOTYPE}[mode]
                        entry.linkname = 'outside'
                        archive.addfile(entry)
                    else:
                        entry.size = 4
                        archive.addfile(entry, io.BytesIO(b'data'))
                        if mode == 'duplicate':
                            archive.addfile(entry, io.BytesIO(b'data'))
                        elif mode == 'multiple-roots':
                            other = tarfile.TarInfo('other/file')
                            other.size = 4
                            archive.addfile(other, io.BytesIO(b'data'))
                with self.subTest(mode=mode), self.assertRaises(ValueError):
                    release_archive.unpack(path, f.root / ('unpack-' + mode))
            good = f.component('app')
            archive = next(good.glob('*.tar.gz'))
            for constant in ('MAX_ARCHIVE', 'MAX_UNPACKED'):
                with patch.object(release_archive, constant, 1), self.assertRaises(ValueError):
                    release_archive.unpack(archive, f.root / ('limit-' + constant))

    def test_every_importing_entrypoint_bare_python_has_no_pycache(self):
        names = ('hourly.py', 'deploy.py', 'deploy-run.py', 'release-entry.py', 'release-dispatch.py',
                 'ci.py', 'benchmark-free-network.py', 'verification_harness.py', 'verify-node.py', 'package-node-windows.py', 'run-delivery-live.py')
        with Fixture() as f:
            for name in names:
                shutil.copy2(SOURCE / name, f.scripts / name)
            for name in names:
                result = subprocess.run([sys.executable, str(f.scripts / name), '--help'],
                    cwd=f.root, env=f.env, capture_output=True, text=True)
                self.assertEqual(result.returncode, 0, (name, result.stderr))
                self.assertEqual(list(f.root.rglob('__pycache__')), [])

    def test_bare_python_minimum_copy_does_not_write_bytecode(self):
        with Fixture() as f:
            self.assertEqual(f.cli('plan','--app','cli',bytecode_flag=False).stdout,IDENTITY)
            self.assertEqual(f.cli('self-test',bytecode_flag=False).stdout,SELF_TEST)
            self.assertEqual(list(f.root.rglob('__pycache__')),[])


if __name__=='__main__':
    unittest.main()
