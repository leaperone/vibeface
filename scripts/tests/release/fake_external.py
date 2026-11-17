"""Strict offline process boundary. Every unrecognized argv fails closed."""
import base64
import hashlib
import json
import os
from pathlib import Path
import sys

root = Path(os.environ['RELEASE_FIXTURE'])
state_path = root / 'state.json'
state = json.loads(state_path.read_text())
args = sys.argv[1:]
tool = Path(sys.argv[0]).name
state['events'].append({'tool': tool, 'args': args})


def save():
    state_path.write_text(json.dumps(state, sort_keys=True))


def finish(value='', code=0, error=''):
    save()
    if isinstance(value, bytes):
        sys.stdout.buffer.write(value)
    elif value:
        print(value)
    if error:
        print(error, file=sys.stderr)
    raise SystemExit(code)


def fault(operation, apply):
    spec = state.get('fault', {})
    hit = spec.get('operation') == operation and not spec.get('used')
    if not hit or spec.get('applied', True):
        apply()
    if hit:
        spec['used'] = True
        finish(code=7, error='fixture unknown response')


def metadata(record):
    return {k: v for k, v in record.items() if k != 'body'}


if tool == 'git':
    if args == ['rev-parse', 'HEAD']:
        finish(state['head'])
    if args == ['status', '--porcelain']:
        finish(state.get('dirty', ''))
    if args == ['rev-parse', '--is-shallow-repository']:
        finish('false')
    if len(args) == 2 and args[0] == 'rev-parse' and args[1].endswith('^{commit}'):
        tag = args[1][:-9]
        finish(state.get('tag_source', {}).get(tag, state['releases'].get(tag, {}).get('source', state['head'])))
    if len(args) == 4 and args[:2] == ['merge-base', '--is-ancestor'] and args[3] == 'origin/main':
        finish(code=0 if args[2] in state['on_main'] else 1)
    if len(args) == 6 and args[:5] == ['fetch', '--quiet', '--tags', 'origin', 'main'] and args[5].startswith('refs/tags/'):
        finish()
elif tool == 'aapt':
    if len(args) == 3 and args[:2] == ['dump', 'badging'] and Path(args[2]).is_file():
        finish(state.get('badging', "package: name='cn.leaperone.vibeface' versionCode='26111409' versionName='1.0.0'"))
elif tool == 'gh':
    if len(args) == 4 and args[:3] == ['api', '--paginate', '--slurp']:
        source = args[3].split('head_sha=', 1)[-1].split('&')[0]
        expected = f'repos/leaperone/vibeface/actions/workflows/ci.yml/runs?head_sha={source}&branch=main&event=push&per_page=100'
        if args[3] == expected and source in state['on_main']:
            record = dict(head_sha=source, head_branch='main', path='.github/workflows/ci.yml', event='push',
                repository={'full_name': 'leaperone/vibeface'}, head_repository={'full_name': 'leaperone/vibeface'},
                status='completed', conclusion='success')
            record.update(state.get('ci_changes', {}))
            finish(json.dumps([{'workflow_runs': [record]}]))
    if len(args) == 2 and args[0] == 'api' and args[1].startswith('repos/leaperone/vibeface/releases/tags/'):
        tag = args[1].split('/tags/')[1]
        record = state['releases'].get(tag)
        if record:
            finish(json.dumps({**record, 'assets': [metadata(a) for a in record['assets']]}))
        finish(code=1, error='release not found')
    if len(args) == 4 and args[:3] == ['api', '-H', 'Accept: application/octet-stream'] and args[3].startswith('repos/leaperone/vibeface/releases/assets/'):
        asset_id = int(args[3].rsplit('/', 1)[1])
        for record in state['releases'].values():
            for asset in record['assets']:
                if asset['id'] == asset_id:
                    finish(base64.b64decode(asset['body']))
    if len(args) >= 5 and args[0] == 'release' and args[3:5] == ['--repo', 'leaperone/vibeface']:
        op, tag = args[1:3]
        record = state['releases'].get(tag)
        if op == 'view' and args[5:] == ['--json', 'isDraft,assets']:
            if state.get('view_error'):
                finish(code=1, error=state['view_error'])
            if record:
                finish(json.dumps({'isDraft': record['draft'], 'assets': [metadata(a) for a in record['assets']]}))
            finish(code=1, error='release not found')
        if op == 'create' and args[5:11] == ['--verify-tag', '--title', tag, '--notes', 'BUILDING; incomplete until release.json is published.', '--latest=false'] and args[11:] in (['--prerelease=true'], ['--prerelease=false']):
            if record:
                finish(code=97, error='duplicate create')
            fault('create', lambda: state['releases'].update({tag: {'tag_name': tag, 'draft': False,
                'prerelease': args[-1] == '--prerelease=true', 'assets': [], 'source': state['head']}}))
            finish()
        if record and op == 'upload' and len(args) == 6:
            path = Path(args[5]); body = path.read_bytes()
            if any(a['name'] == path.name for a in record['assets']):
                finish(code=97, error='duplicate upload')
            def upload():
                record['assets'].append(dict(id=len(record['assets']) + 1, name=path.name, size=len(body),
                    digest='sha256:' + hashlib.sha256(body).hexdigest(), body=base64.b64encode(body).decode()))
            fault('upload:' + path.name, upload)
            finish()
        if record and op == 'edit' and len(args) == 8 and args[5] == '--notes' and args[-1] == '--latest=false':
            fault('edit', lambda: record.update(notes=args[6]))
            finish()
        if record and op == 'download' and len(args) == 9 and args[5] == '--pattern' and args[7] == '--dir':
            for asset in record['assets']:
                if asset['name'] == args[6]:
                    (Path(args[8]) / args[6]).write_bytes(base64.b64decode(asset['body']))
                    finish()
finish(code=99, error='unrecognized offline fixture command: ' + tool + ' ' + repr(args))
