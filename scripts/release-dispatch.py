#!/usr/bin/env python3
"""Dispatch one exact deployment, reconcile its run, and wait for its real result."""
import argparse
import datetime
import json
import subprocess
import time
import release

WORKFLOWS = {'core': 'deploy-node.yml', 'app': 'deploy-app.yml', 'www': 'deploy-www.yml'}
LABELS = {'core': 'Node', 'app': 'Web App', 'www': 'Website'}


def gh_read(*args):
    return json.loads(subprocess.check_output(['gh', *args], text=True))


def wait_for_deployment(app, tag, read=gh_read, dispatch=None, pause=time.sleep, attempts=360):
    release.require(app in WORKFLOWS and release.parse_tag(tag)[0] == app, 'deployment target/tag differs')
    workflow = WORKFLOWS[app]
    started = datetime.datetime.now(datetime.timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ')
    command = ['gh', 'workflow', 'run', workflow, '--repo', release.GITHUB_REPO, '--ref', 'main',
               '--raw-field', 'tag=' + tag, '--raw-field', 'apply=true']
    dispatch = dispatch or (lambda args: subprocess.run(args, check=True))
    try:
        dispatch(command)
    except (OSError, subprocess.SubprocessError):
        # The write may already have succeeded. Observe instead of repeating it.
        pass
    run_id = None
    for _ in range(attempts):
        try:
            if run_id is None:
                runs = read('run', 'list', '--repo', release.GITHUB_REPO, '--workflow', workflow, '--branch', 'main',
                            '--event', 'workflow_dispatch', '--limit', '100', '--json', 'databaseId,displayTitle,createdAt')
                title = f'Deploy {LABELS[app]} from {tag} (apply, main)'
                matches = [r for r in runs if r['createdAt'] >= started and r['displayTitle'] == title]
                release.require(len(matches) <= 1, 'ambiguous deployment dispatch; reconcile matching runs')
                if matches:
                    run_id = matches[0]['databaseId']
                    print(f'Deployment: https://github.com/{release.GITHUB_REPO}/actions/runs/{run_id}', flush=True)
            if run_id is not None:
                state = read('run', 'view', str(run_id), '--repo', release.GITHUB_REPO, '--json', 'status,conclusion')
                if state['status'] == 'completed':
                    release.require(state['conclusion'] == 'success', 'deployment completed with ' + str(state['conclusion']))
                    return run_id
        except (OSError, subprocess.SubprocessError, json.JSONDecodeError):
            pass
        pause(5)
    raise ValueError('deployment outcome unconfirmed; inspect runs before retrying')


def self_test():
    tag = 'core-v0.0.1-hourly.2026111409'
    writes, calls = [], []
    sequence = iter([subprocess.CalledProcessError(1, ['gh']), [],
                     [{'databaseId': 42, 'createdAt': '9999', 'displayTitle': f'Deploy Node from {tag} (apply, main)'}],
                     {'status': 'queued', 'conclusion': ''}, {'status': 'in_progress', 'conclusion': None},
                     {'status': 'completed', 'conclusion': 'success'}])
    def read(*args):
        calls.append(args)
        result = next(sequence)
        if isinstance(result, Exception):
            raise result
        return result
    assert wait_for_deployment('core', tag, read, writes.append, lambda _: None, attempts=8) == 42
    assert len(writes) == 1
    def failed(*args):
        return [{'databaseId':42,'createdAt':'9999','displayTitle':f'Deploy Node from {tag} (apply, main)'}] if args[:2] == ('run','list') else {'status':'completed','conclusion':'failure'}
    try:
        wait_for_deployment('core', tag, failed, writes.append, lambda _: None, attempts=2)
    except ValueError as error:
        assert 'failure' in str(error)
    else:
        raise AssertionError('failed child deployment accepted')
    assert len(writes) == 2
    for app, title in (('app', 'Web App'), ('www', 'Website')):
        selected_tag = app + '-v0.0.1'
        def terminal(*args):
            if args[:2] == ('run', 'list'):
                return [{'databaseId': 43, 'createdAt': '9999',
                         'displayTitle': f'Deploy {title} from {selected_tag} (apply, main)'}]
            return {'status': 'completed', 'conclusion': 'success'}
        assert wait_for_deployment(app, selected_tag, terminal, writes.append, lambda _: None, attempts=1) == 43
    print('Deployment dispatch self-test passed: transient discovery, queued state and failed child')


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--app', choices=WORKFLOWS)
    parser.add_argument('--tag')
    parser.add_argument('--self-test', action='store_true')
    args = parser.parse_args()
    self_test() if args.self_test else wait_for_deployment(args.app, args.tag)


if __name__ == '__main__':
    main()
