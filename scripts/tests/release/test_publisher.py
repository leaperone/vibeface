#!/usr/bin/env python3
"""Progressive publication, unknown writes and immutable public-reader corpus."""
import sys
sys.dont_write_bytecode = True
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
import base64
import copy
import hashlib
import json
import os
import unittest
from unittest.mock import patch
import release_context
import release_publisher
import release_format
from tests.release.support import Fixture, SOURCE, VERSIONS, write_json, pin, CONTRACTS

CORPUS = json.loads(Path(__file__).with_name('fixtures').joinpath('publication-cases.json').read_text())


def writes(state):
    return [e['args'] for e in state['events'] if e['tool']=='gh' and e['args'][:1]==['release'] and e['args'][1] in ('create','upload','edit')]


def counts(state):
    events=writes(state)
    return tuple(sum(a[1]==name for a in events) for name in ('create','upload','edit'))


def publish(f,out,code=0):
    result=f.cli('publish','app',out,code=code)
    expected=dict(app='app',version='3.4.5-rc.2',tag='app-v3.4.5-rc.2',source_commit='a'*40,channel='rc',ci_source_commit='a'*40)
    if result.stdout!=json.dumps(expected)+'\n':raise AssertionError(result.stdout)
    return result


def marker_change(record,change):
    asset=next(a for a in record['assets'] if a['name']=='release.json')
    data=json.loads(base64.b64decode(asset['body']))
    change(data)
    body=(json.dumps(data,indent=2,sort_keys=True)+'\n').encode()
    asset.update(body=base64.b64encode(body).decode(),size=len(body),digest='sha256:'+hashlib.sha256(body).hexdigest())


class PublisherContracts(unittest.TestCase):
    def test_success_public_create_payload_order_marker_then_notes(self):
        with Fixture() as f:
            out=f.component('app')
            # Legal manifests need not sort assets; publisher preserves the given order.
            marker=out/'release.json';info=json.loads(marker.read_text());info['assets'].reverse();write_json(marker,info)
            publish(f,out)
            state=f.state();self.assertEqual(counts(state),(1,3,1))
            operations=writes(state)
            self.assertEqual([Path(a[-1]).name for a in operations if a[1]=='upload'],[a['name'] for a in info['assets']]+['release.json'])
            self.assertIn('--latest=false',operations[0]);self.assertNotIn('--draft',operations[0]);self.assertIn('BUILDING; incomplete until release.json is published.',operations[0])
            self.assertEqual(operations[-1][1],'edit')
            with patch.object(release_context,'ROOT',f.root),patch.dict(os.environ,f.env,clear=True):
                public,_,_=release_publisher.published(info['tag'])
                self.assertEqual(public,info)

    def test_unknown_create_upload_marker_and_edit_across_invocations(self):
        cases = [(c['operation'], c['applied'], tuple(c['first_counts']), tuple(c['final_counts'])) for c in CORPUS['unknown_writes']]
        for operation,applied,first,second in cases:
            with self.subTest(operation=operation,applied=applied),Fixture() as f:
                out=f.component('app');info=json.loads((out/'release.json').read_text());first_name=info['assets'][0]['name']
                op=operation.replace('payload',first_name)
                f.change(fault={'operation':op,'applied':applied})
                publish(f,out,code=1)
                state=f.state();self.assertEqual(counts(state),first)
                boundary=len(state['events'])
                publish(f,out)
                state=f.state();self.assertEqual(counts(state),second)
                subsequent=state['events'][boundary:]
                gh=[e['args'] for e in subsequent if e['tool']=='gh']
                self.assertEqual(gh[1][:2] if gh[0][0]=='api' else gh[0][:2],['release','view'])
                first_write=next(i for i,e in enumerate(subsequent) if e['args'] in writes({'events':subsequent}))
                self.assertTrue(any(e['args'][:2]==['release','view'] for e in subsequent[:first_write]))
                uploads=[a for a in writes(state) if a[1]=='upload']
                attempts=sum(Path(a[-1]).name==first_name for a in uploads)
                self.assertEqual(attempts,2 if operation=='upload:payload' and not applied else 1)
                record=state['releases'][info['tag']]
                self.assertEqual({a['name'] for a in record['assets']},{a['name'] for a in info['assets']}|{'release.json'})
                self.assertEqual(len(record['assets']),3)

    def test_unknown_view_never_creates(self):
        for error in ('timeout','HTTP 403','network disconnected'):
            with self.subTest(error=error),Fixture() as f:
                out=f.component('app');f.change(view_error=error)
                result=publish(f,out,code=1)
                self.assertIn('reconcile before publishing',result.stderr)
                self.assertEqual(counts(f.state()),(0,0,0))

    def test_existing_bytes_and_missing_digest_are_reconciled(self):
        for digest_available in (True,False):
            with self.subTest(digest=digest_available),Fixture() as f:
                out=f.component('app');info=f.remote(out)
                state=f.state()
                if not digest_available:
                    for asset in state['releases'][info['tag']]['assets']:asset.pop('digest')
                    f.save(state)
                publish(f,out)
                state=f.state();self.assertEqual(counts(state),(0,0,1))
                self.assertEqual(sum(e['args'][:2]==['release','download'] for e in state['events']),0 if digest_available else 3)

    def test_foreign_duplicate_draft_and_changed_immutable_bytes_write_zero(self):
        for mode in ('foreign','duplicate','draft','size','digest','bytes-no-digest'):
            with self.subTest(mode=mode),Fixture() as f:
                out=f.component('app');info=f.remote(out);state=f.state();record=state['releases'][info['tag']]
                if mode=='foreign':record['assets'].append(dict(record['assets'][0],name='foreign'))
                elif mode=='duplicate':record['assets'].append(copy.deepcopy(record['assets'][0]))
                elif mode=='draft':record['draft']=True
                elif mode=='size':record['assets'][0]['size']+=1
                elif mode=='digest':record['assets'][0]['digest']='sha256:'+'0'*64
                else:record['assets'][0].pop('digest');record['assets'][0]['body']=base64.b64encode(b'different').decode()
                f.save(state);publish(f,out,code=1)
                self.assertEqual(counts(f.state()),(0,0,0))

    def test_public_reader_marker_metadata_identity_source_and_ci_rejections(self):
        modes = CORPUS['reader_rejections']
        for mode in modes:
            with self.subTest(mode=mode),Fixture() as f:
                out=f.component('app');info=f.remote(out);state=f.state();record=state['releases'][info['tag']]
                marker=next(a for a in record['assets'] if a['name']=='release.json')
                payload=next(a for a in record['assets'] if a['name']!='release.json')
                if mode=='no-marker':record['assets'].remove(marker)
                elif mode=='huge-marker':marker['size']=1024*1024+1
                elif mode=='marker-size':marker['size']+=1
                elif mode=='marker-digest':marker['digest']='sha256:'+'0'*64
                elif mode=='marker-bytes':marker['body']=base64.b64encode(b'{}').decode()
                elif mode=='duplicate':record['assets'].append(copy.deepcopy(payload))
                elif mode=='foreign':record['assets'].append(dict(payload,name='foreign'))
                elif mode=='missing-payload':record['assets'].remove(payload)
                elif mode=='draft':record['draft']=True
                elif mode=='prerelease':record['prerelease']=False
                elif mode=='tag':record['tag_name']='www-v1.0.0'
                elif mode.startswith('manifest-'):
                    key=mode[9:];value={'app':'www','version':'9.9.9','channel':'stable','source':'invalid','extra':1}[key]
                    marker_change(record,lambda d:d.update(**{('source_commit' if key=='source' else key):value}))
                elif mode=='contracts':marker_change(record,lambda d:d['contracts'].update(data_epoch=2))
                elif mode=='schema-bool':marker_change(record,lambda d:d.update(schema=True))
                elif mode=='asset-size':payload['size']+=1
                elif mode=='asset-digest':payload['digest']='sha256:'+'0'*64
                elif mode=='tag-source':state['tag_source']={info['tag']:'b'*40}
                elif mode.startswith('ci-'):
                    key=mode[3:];key={'sha':'head_sha','branch':'head_branch','repo':'repository','head-repo':'head_repository','path':'path','event':'event','status':'status','conclusion':'conclusion'}[key]
                    value={'head_sha':'b'*40,'head_branch':'feature','repository':{'full_name':'other/repo'},'head_repository':{'full_name':'other/repo'},'path':'.github/workflows/other.yml','event':'workflow_dispatch','status':'in_progress','conclusion':'failure'}[key]
                    state['ci_changes']={key:value}
                f.save(state)
                with patch.object(release_context,'ROOT',f.root),patch.dict(os.environ,f.env,clear=True),self.assertRaises(ValueError):
                    release_publisher.published(info['tag'])
                self.assertEqual(counts(f.state()),(0,0,0))

    def test_dependency_pin_every_field_is_exact(self):
        with Fixture() as f:
            out=f.component('app');info=f.remote(out)
            good=pin('app',source='a'*40,sha=hashlib.sha256((out/'release.json').read_bytes()).hexdigest())
            with patch.object(release_context,'ROOT',f.root),patch.dict(os.environ,f.env,clear=True):
                self.assertEqual(release_publisher.published(info['tag'],good)[0],info)
                for key in good:
                    bad=dict(good);bad[key]={'tag':'app-v9.9.9','version':'9.9.9','source_commit':'b'*40,'manifest_sha256':'0'*64}[key]
                    with self.subTest(key=key),self.assertRaises(ValueError):release_publisher.published(info['tag'],bad)

    def test_shared_completion_fixture_and_stable_dependency_rule(self):
        shared = json.loads((SOURCE / 'fixtures/cli-release-completion.json').read_text())
        self.assertEqual(release_format.verify_release_identity(shared, CONTRACTS), shared)
        self.assertEqual(len(shared['assets']) + 1, 9)
        stable = copy.deepcopy(shared)
        stable.update(version='1.2.3', tag='cli-v1.2.3', channel='stable')
        with self.assertRaisesRegex(ValueError, 'stable releases require stable dependency pins'):
            release_format.verify_release_identity(stable, CONTRACTS)


if __name__=='__main__':
    unittest.main()
