#!/usr/bin/env python3
"""Minimal gh/git stub for progressive publication CLI subprocess tests."""
import base64
import hashlib
import json
import os
import subprocess
import sys
from pathlib import Path

STATE_PATH = Path(os.environ['VIBEFACE_TEST_GH_STATE'])


def load():
    state = json.loads(STATE_PATH.read_text(encoding='utf-8'))
    state['assets'] = {name: base64.b64decode(data) for name, data in state.get('assets', {}).items()}
    return state


def save(state):
    payload = dict(state)
    payload['assets'] = {name: base64.b64encode(data).decode('ascii') for name, data in state['assets'].items()}
    STATE_PATH.write_text(json.dumps(payload, indent=2, sort_keys=True) + '\n', encoding='utf-8')


def digest_bytes(data):
    return hashlib.sha256(data).hexdigest()


def gh_main(argv):
    state = load()
    if argv[0] != 'gh':
        raise AssertionError(argv)
    if argv[1] == 'api':
        path = argv[2]
        if path.endswith(f"/commits/{state['tag']}"):
            print(json.dumps(dict(sha=state['source'])))
            return 0
        if path.endswith(f"/releases/tags/{state['tag']}"):
            if not state.get('exists'):
                sys.stderr.write('release not found\n')
                return 1
            assets = []
            for name, data in sorted(state['assets'].items()):
                assets.append(dict(name=name, size=len(data), digest=f'sha256:{digest_bytes(data)}'))
            print(json.dumps(dict(assets=assets, immutable=state.get('immutable', False))))
            return 0
        raise AssertionError(path)
    if argv[1:2] == ['release'] and len(argv) > 2:
        action = argv[2]
    else:
        action = argv[1]
    if action == 'view':
        if not state.get('exists'):
            sys.stderr.write('release not found\n')
            return 1
        print(json.dumps(dict(isDraft=state.get('draft', False), body=state.get('body', ''),
                              assets=[{'name': name} for name in sorted(state['assets'])])))
        return 0
    if action == 'create':
        state['exists'] = True
        state['draft'] = '--draft=false' not in argv
        if '--notes' in argv:
            state['body'] = argv[argv.index('--notes') + 1]
        save(state)
        return 0
    if argv[1:3] == ['release', 'upload']:
        for arg in argv[3:]:
            if arg.startswith('-') or arg == state['tag']:
                continue
            path = Path(arg)
            state['assets'][path.name] = path.read_bytes()
        save(state)
        return 0
    if action == 'download':
        name = argv[argv.index('--pattern') + 1]
        dest = Path(argv[argv.index('--dir') + 1])
        (dest / name).write_bytes(state['assets'][name])
        return 0
    if action == 'edit':
        if '--notes' in argv:
            state['body'] = argv[argv.index('--notes') + 1]
        save(state)
        return 0
    raise AssertionError(argv)


def git_main(argv):
    state = load()
    if argv[1] == 'merge-base':
        return 0
    if argv[1] == 'rev-parse':
        target = argv[2] if len(argv) > 2 else ''
        if target.endswith('^{commit}') or target == 'HEAD':
            print(state['source'])
            return 0
        print('clean')
        return 0
    if argv[1] == 'status' and len(argv) > 2 and argv[2] == '--porcelain':
        return 0
    raise AssertionError(argv)


def main():
    role = os.environ.get('VIBEFACE_TEST_TRANSPORT_ROLE') or Path(sys.argv[0]).name
    argv = ['gh' if role == 'gh' else 'git', *sys.argv[1:]]
    if role == 'gh':
        raise SystemExit(gh_main(argv))
    if role == 'git':
        raise SystemExit(git_main(argv))
    raise AssertionError(argv)


if __name__ == '__main__':
    main()
