#!/usr/bin/env python3
"""Validate a fixed application's release source and publication intent."""
import argparse
import os
import release


def resolve_entry(app, environ=None):
    env = os.environ if environ is None else environ
    release.require(app in release.APPS, 'unknown release application')
    tag, intent = env['RELEASE_TAG'], env['PUBLISH']
    release.require(intent in ('true', 'false'), 'PUBLISH must be true or false')
    publish = intent == 'true'
    if publish:
        release.require(env['GITHUB_REPOSITORY'] == release.GITHUB_REPO, 'publication requires the official repository')
        release.require(tag and env['GITHUB_REF'] == 'refs/tags/' + tag, 'publication requires exact tag ref')
    result = release.plan(app, tag or None, require_main=publish, publishing=publish)
    release.require(result['source_commit'] == env['GITHUB_SHA'], 'build source differs from workflow commit')
    if result['channel'] == 'stable':
        if app in ('core', 'cli', 'desktop'):
            release.require(env.get('WINDOWS_SIGNING_CONFIGURED') == 'true', 'stable native release requires Windows signing')
        if app == 'mobile':
            release.require(env.get('IOS_SIGNING_CONFIGURED') == 'true', 'stable Mobile release requires iOS distribution signing')
    result['publish'] = publish
    return result


def write_outputs(result, output_path):
    with open(output_path, 'a') as output:
        for key in ('app', 'version', 'tag', 'source_commit', 'channel'):
            value = result[key]
            release.require(isinstance(value, str) and '\n' not in value and '\r' not in value, 'identity output must be one line')
            output.write(f'{key}={value}\n')
        output.write('publish=' + str(result['publish']).lower() + '\n')
        if result['app'] == 'desktop':
            pins = release.dependencies('desktop', required=True)
            output.write('runtime_tag=' + pins['cli']['tag'] + '\n')


def self_test():
    from unittest.mock import patch
    source = 'a' * 40
    for app in release.APPS:
        context = dict(app=app, version='0.0.1-hourly.2026111409', channel='hourly', tag=app+'-v0.0.1-hourly.2026111409', source_commit=source)
        env = dict(RELEASE_TAG=context['tag'], PUBLISH='true', GITHUB_REPOSITORY=release.GITHUB_REPO,
                   GITHUB_REF='refs/tags/'+context['tag'], GITHUB_SHA=source)
        with patch.object(release, 'plan', return_value=context.copy()):
            assert resolve_entry(app, env)['publish'] is True
            for change in ({'GITHUB_REPOSITORY':'fork/repo'}, {'GITHUB_REF':'refs/heads/main'}, {'PUBLISH':'yes'}, {'GITHUB_SHA':'b'*40}):
                try:
                    resolve_entry(app, env | change)
                except ValueError:
                    pass
                else:
                    raise AssertionError('untrusted publication context accepted')
    print('Independent release entry self-test passed')


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--app', choices=release.APPS)
    parser.add_argument('--self-test', action='store_true')
    args = parser.parse_args()
    if args.self_test:
        self_test()
    else:
        write_outputs(resolve_entry(args.app), os.environ['GITHUB_OUTPUT'])


if __name__ == '__main__':
    main()
