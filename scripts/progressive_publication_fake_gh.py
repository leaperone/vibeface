#!/usr/bin/env python3
"""Shared gh/git transport for progressive publication tests and CLI subprocesses."""
import base64
import hashlib
import json
import os
import subprocess
import sys
from pathlib import Path


def require(condition, message):
    if not condition:
        raise AssertionError(message)


class FakeRelease:
    def __init__(self, tag, source):
        self.tag = tag
        self.source = source
        self.exists = False
        self.draft = False
        self.body = ''
        self.immutable = False
        self.assets = {}
        self.uploads = []
        self.edits = []

    def asset_records(self):
        return [{'name': name, 'size': len(data), 'digest': f'sha256:{digest_bytes(data)}'}
                for name, data in sorted(self.assets.items())]

    def view_json(self):
        return json.dumps(dict(isDraft=self.draft, body=self.body,
                               assets=[{'name': name} for name in sorted(self.assets)]))

    def api_payload(self):
        return json.dumps(dict(assets=self.asset_records(), immutable=self.immutable))


def digest_bytes(data):
    return hashlib.sha256(data).hexdigest()


def gh_handler(state, calls):
    def runner(args, **kwargs):
        command = list(args)
        calls.append(command)
        if command[:2] == ['git', 'merge-base']:
            return subprocess.CompletedProcess(command, 0, '', '')
        if command[:2] == ['git', 'rev-parse']:
            if command[-1].endswith('^{commit}'):
                return subprocess.CompletedProcess(command, 0, state.source, '')
            if command[2] == 'HEAD':
                return subprocess.CompletedProcess(command, 0, state.source, '')
            return subprocess.CompletedProcess(command, 0, 'clean', '')
        if command[0] == 'git' and command[1] == 'status':
            return subprocess.CompletedProcess(command, 0, '', '')
        require(command[0] == 'gh', 'unexpected command')
        if command[1] == 'api':
            path = command[2]
            if path.endswith(f'/commits/{state.tag}'):
                return subprocess.CompletedProcess(command, 0, json.dumps(dict(sha=state.source)), '')
            if path.endswith(f'/releases/tags/{state.tag}'):
                require(state.exists, 'release missing')
                return subprocess.CompletedProcess(command, 0, state.api_payload(), '')
            raise AssertionError(path)
        action = command[2] if command[1] == 'release' else command[1]
        if action == 'view':
            if not state.exists:
                return subprocess.CompletedProcess(command, 1, '', 'release not found')
            return subprocess.CompletedProcess(command, 0, state.view_json(), '')
        if action == 'create':
            state.exists = True
            state.draft = '--draft=false' not in command
            if '--notes' in command:
                state.body = command[command.index('--notes') + 1]
            return subprocess.CompletedProcess(command, 0, '', '')
        if action == 'upload':
            for path in (Path(arg) for arg in command[3:] if not arg.startswith('-') and arg != state.tag):
                state.uploads.append(path.name)
                state.assets[path.name] = path.read_bytes()
            return subprocess.CompletedProcess(command, 0, '', '')
        if action == 'download':
            name = command[command.index('--pattern') + 1]
            destination = Path(command[command.index('--dir') + 1])
            (destination / name).write_bytes(state.assets[name])
            return subprocess.CompletedProcess(command, 0, '', '')
        if action == 'edit':
            if '--notes' in command:
                state.body = command[command.index('--notes') + 1]
            state.edits.append(command)
            return subprocess.CompletedProcess(command, 0, '', '')
        raise AssertionError(command)

    def runner_output(*args, **kwargs):
        return runner(args, **kwargs).stdout

    return runner, runner_output


def main():
    path = Path(os.environ['VIBEFACE_TEST_GH_STATE'])
    payload = json.loads(path.read_text(encoding='utf-8'))
    state = FakeRelease(payload.pop('tag'), payload.pop('source'))
    vars(state).update(payload)
    state.assets = {name: base64.b64decode(data) for name, data in state.assets.items()}
    role = os.environ.get('VIBEFACE_TEST_TRANSPORT_ROLE') or Path(sys.argv[0]).name
    require(role in ('gh', 'git'), 'unexpected transport role')
    result = gh_handler(state, [])[0]([role, *sys.argv[1:]])
    payload = dict(vars(state), assets={name: base64.b64encode(data).decode('ascii') for name, data in state.assets.items()})
    path.write_text(json.dumps(payload, indent=2, sort_keys=True) + '\n', encoding='utf-8')
    sys.stdout.write(result.stdout)
    sys.stderr.write(result.stderr)
    return result.returncode


if __name__ == '__main__':
    raise SystemExit(main())
