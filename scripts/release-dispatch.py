#!/usr/bin/env python3
"""Dispatch one exact deployment, reconcile its run, and wait for its real result."""

import sys
sys.dont_write_bytecode = True
import release_format
import argparse
import json
import secrets
import subprocess
import time


WORKFLOWS = {'core': 'deploy-node.yml', 'app': 'deploy-app.yml', 'www': 'deploy-www.yml'}
LABELS = {'core': 'Node', 'app': 'Web App', 'www': 'Website'}


def gh_read(*args):
    return json.loads(subprocess.check_output(['gh', *args], text=True))


def wait_for_deployment(app, tag, read=gh_read, dispatch=None, pause=time.sleep, attempts=360):
    release_format.require(app in WORKFLOWS and release_format.parse_tag(tag)[0] == app, 'deployment target/tag differs')
    workflow = WORKFLOWS[app]
    request_id = secrets.token_hex(16)
    title = f'Deploy {LABELS[app]} from {tag} (apply, main) [request {request_id}]'
    command = ['gh', 'workflow', 'run', workflow, '--repo', release_format.GITHUB_REPO, '--ref', 'main',
               '--raw-field', 'tag=' + tag, '--raw-field', 'apply=true', '--raw-field', 'request_id=' + request_id]
    dispatch = dispatch or (lambda args: subprocess.run(args, check=True))
    print(f'Deployment request: {request_id} ({workflow}, {tag}, main)', flush=True)
    try:
        dispatch(command)
    except (OSError, subprocess.SubprocessError):
        # The write may already have succeeded. Observe instead of repeating it.
        pass
    run_id = None
    for _ in range(attempts):
        try:
            if run_id is None:
                runs = read('run', 'list', '--repo', release_format.GITHUB_REPO, '--workflow', workflow, '--branch', 'main',
                            '--event', 'workflow_dispatch', '--limit', '100', '--json', 'databaseId,displayTitle')
                matches = [r for r in runs if r['displayTitle'] == title]
                release_format.require(len(matches) <= 1, 'ambiguous deployment dispatch; reconcile matching runs')
                if matches:
                    run_id = matches[0]['databaseId']
                    print(f'Deployment: https://github.com/{release_format.GITHUB_REPO}/actions/runs/{run_id}', flush=True)
            if run_id is not None:
                state = read('run', 'view', str(run_id), '--repo', release_format.GITHUB_REPO, '--json', 'status,conclusion')
                if state['status'] == 'completed':
                    release_format.require(state['conclusion'] == 'success', 'deployment completed with ' + str(state['conclusion']))
                    return run_id
        except (OSError, subprocess.SubprocessError, json.JSONDecodeError):
            pass
        pause(5)
    raise ValueError('deployment outcome unconfirmed; inspect runs before retrying')


def self_test():
    import contextlib
    import io

    request_ids = set()
    scenarios = ('success', 'lost-response', 'unconfirmed', 'ambiguous', 'failure', 'cancelled')
    for app, workflow, label in (('core', 'deploy-node.yml', 'Node'),
                                 ('app', 'deploy-app.yml', 'Web App'),
                                 ('www', 'deploy-www.yml', 'Website')):
        tag = app + '-v0.0.1-hourly.2026111409'
        for scenario in scenarios:
            writes, pauses = [], []
            output = io.StringIO()
            responses = iter(())

            def dispatch(command):
                nonlocal responses
                writes.append(command)
                request_id = command[-1].removeprefix('request_id=')
                assert request_id and request_id not in request_ids
                request_ids.add(request_id)
                assert f'Deployment request: {request_id} ({workflow}, {tag}, main)' in output.getvalue()
                assert command == ['gh', 'workflow', 'run', workflow, '--repo', release_format.GITHUB_REPO,
                                   '--ref', 'main', '--raw-field', 'tag=' + tag, '--raw-field', 'apply=true',
                                   '--raw-field', 'request_id=' + request_id]
                title = f'Deploy {label} from {tag} (apply, main) [request {request_id}]'
                run = {'databaseId': 42, 'displayTitle': title}
                unrelated = [
                    {'databaseId': 7, 'displayTitle': f'Deploy {label} from {tag} (apply, main) [request other]'},
                    {'databaseId': 8, 'displayTitle': f'Deploy {label} from {tag}0 (apply, main) [request {request_id}]'},
                    {'databaseId': 9, 'displayTitle': f'Deploy {label} from {tag} (apply, main)'},
                ]
                if scenario == 'unconfirmed':
                    responses = iter([('list', unrelated)] * 8)
                elif scenario == 'ambiguous':
                    responses = iter([('list', [run, {'databaseId': 43, 'displayTitle': title}])])
                elif scenario in ('failure', 'cancelled'):
                    responses = iter([('list', [run]), ('view', {'status': 'completed', 'conclusion': scenario})])
                else:
                    responses = iter([
                        ('list', subprocess.CalledProcessError(1, ['gh'])), ('list', []), ('list', unrelated),
                        ('list', unrelated + [run]), ('view', {'status': 'queued', 'conclusion': None}),
                        ('view', json.JSONDecodeError('temporary read failure', '', 0)),
                        ('view', {'status': 'in_progress', 'conclusion': None}),
                        ('view', {'status': 'completed', 'conclusion': 'success'}),
                    ])
                if scenario == 'lost-response':
                    raise subprocess.CalledProcessError(1, ['gh'])

            def read(*args):
                operation, result = next(responses)
                if operation == 'list':
                    assert args == ('run', 'list', '--repo', release_format.GITHUB_REPO, '--workflow', workflow,
                                    '--branch', 'main', '--event', 'workflow_dispatch', '--limit', '100',
                                    '--json', 'databaseId,displayTitle')
                else:
                    assert args == ('run', 'view', '42', '--repo', release_format.GITHUB_REPO,
                                    '--json', 'status,conclusion')
                if isinstance(result, Exception):
                    raise result
                return result

            with contextlib.redirect_stdout(output):
                try:
                    result = wait_for_deployment(app, tag, read, dispatch, pauses.append, attempts=8)
                except ValueError as error:
                    expected = {'unconfirmed': 'outcome unconfirmed', 'ambiguous': 'ambiguous deployment',
                                'failure': 'completed with failure', 'cancelled': 'completed with cancelled'}
                    assert scenario in expected and expected[scenario] in str(error), str(error)
                else:
                    assert scenario in ('success', 'lost-response') and result == 42
                    assert len(pauses) == 6
                    assert f'Deployment: https://github.com/{release_format.GITHUB_REPO}/actions/runs/42' in output.getvalue()
            assert len(writes) == 1
            assert next(responses, None) is None, 'deployment returned before its terminal state'
    print('Deployment dispatch self-test passed: 18 cases across core/app/www; exact request correlation, '
          'delayed reads, one write after a lost response, ambiguity, timeout, failure and cancellation')


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--app', choices=WORKFLOWS)
    parser.add_argument('--tag')
    parser.add_argument('--self-test', action='store_true')
    args = parser.parse_args()
    self_test() if args.self_test else wait_for_deployment(args.app, args.tag)


if __name__ == '__main__':
    main()
