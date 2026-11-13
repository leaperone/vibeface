#!/usr/bin/env python3
"""Validate the fixed product or website workflow identity before producers run."""
import argparse
import os

import release


def resolve_entry(app, environ=None):
    environ = os.environ if environ is None else environ
    release.require(app in ('product', 'www'), 'release entry must be product or www')
    tag = environ['RELEASE_TAG']
    intent = environ['PUBLISH']
    release.require(intent in ('true', 'false'), 'PUBLISH must be true or false')
    publish = intent == 'true'
    if publish:
        release.require(environ['GITHUB_REPOSITORY'] == release.GITHUB_REPO,
                        'Publication requires the official release repository')
        release.require(bool(tag) and environ['GITHUB_REF'] == 'refs/tags/' + tag,
                        'Publishing requires the exact existing tag as the workflow ref')
    result = release.plan(app, tag=tag or None, require_main=publish, publishing=publish)
    if app == 'product' and result['channel'] == 'stable':
        release.require(environ.get('WINDOWS_SIGNING_CONFIGURED') == 'true',
                        'Stable product builds require a Windows Authenticode certificate before any producers start')
        release.require(environ.get('IOS_SIGNING_CONFIGURED') == 'true',
                        'Stable product builds require the iOS distribution identity and provisioning profile')
    release.require(result['source_commit'] == environ['GITHUB_SHA'],
                    'Build source differs from the workflow commit')
    if publish:
        release.require_ci(result)
    return {**result, 'publish': publish}


def write_outputs(result, output_path):
    values = []
    for key in ('app', 'version', 'tag', 'source_commit', 'channel'):
        value = result[key]
        release.require(isinstance(value, str) and '\n' not in value and '\r' not in value,
                        'Release outputs must be single-line identity strings')
        values.append(f'{key}={value}\n')
    release.require(isinstance(result['publish'], bool), 'Publication intent must be boolean')
    values.append('publish=' + str(result['publish']).lower() + '\n')
    with open(output_path, 'a') as output:
        output.writelines(values)


def self_test():
    import contextlib
    import io
    from pathlib import Path
    import tempfile
    from unittest.mock import patch

    source = 'a' * 40
    context = dict(app='product', version='0.0.1-hourly.2026111109',
                   tag='v0.0.1-hourly.2026111109', source_commit=source, channel='hourly')
    environment = dict(RELEASE_TAG=context['tag'], PUBLISH='true',
                       GITHUB_REPOSITORY=release.GITHUB_REPO,
                       GITHUB_REF='refs/tags/' + context['tag'], GITHUB_SHA=source,
                       WINDOWS_SIGNING_CONFIGURED='true', IOS_SIGNING_CONFIGURED='true')
    provenance = dict(ci_source_commit='b' * 40, ci_run_id='123')
    accepted = rejected = 0

    def exercise(app='product', env=None, identity=None, error=None, fault=None):
        nonlocal accepted, rejected
        data = {**context, 'app': app, **(identity or {})}
        supplied = {**environment, **(env or {})}

        def run(*args, **kwargs):
            if args == ('git', 'status', '--porcelain'):
                return ' M file' if fault == 'dirty' else ''
            if args == ('git', 'rev-parse', data['tag'] + '^{commit}'):
                return 'c' * 40 if fault == 'tag' else source
            raise AssertionError(f'Unexpected command: {args}')

        with contextlib.redirect_stdout(io.StringIO()), \
                patch.object(release, 'identity', return_value=data), \
                patch.object(release, 'run', side_effect=run), \
                patch.object(release, 'validate_source', return_value=provenance) as validate, \
                patch.object(release, 'require_ci') as ci:
            if fault == 'source':
                validate.side_effect = ValueError('invalid source provenance')
            if fault == 'ci':
                ci.side_effect = ValueError('missing successful CI')
            try:
                result = resolve_entry(app, supplied)
            except ValueError as failure:
                release.require(error is not None and error in str(failure),
                                f'Unexpected entry rejection: {failure}')
                rejected += 1
                return
            release.require(error is None, f'Unsafe entry accepted: {error}')
            publishing = supplied['PUBLISH'] == 'true'
            release.require(result == {**data, **(provenance if publishing else {}), 'publish': publishing},
                            'Entry returned the wrong release identity')
            if publishing:
                validate.assert_called_once_with(source, app)
                ci.assert_called_once_with({**data, **provenance})
            else:
                validate.assert_not_called()
                ci.assert_not_called()
            accepted += 1

    exercise()
    website_tag = 'www-v0.0.1-rc.1'
    exercise('www', env=dict(RELEASE_TAG=website_tag, GITHUB_REF='refs/tags/' + website_tag,
                             WINDOWS_SIGNING_CONFIGURED='false', IOS_SIGNING_CONFIGURED='false'),
             identity=dict(tag=website_tag, version='0.0.1-rc.1', channel='rc'))
    exercise(env=dict(RELEASE_TAG='', PUBLISH='false', GITHUB_REPOSITORY='fork/repo',
                      GITHUB_REF='refs/heads/topic'))
    for env, error in (
        (dict(GITHUB_REPOSITORY='fork/repo'), 'official release repository'),
        (dict(RELEASE_TAG=''), 'exact existing tag'),
        (dict(GITHUB_REF='refs/heads/main'), 'exact existing tag'),
        (dict(GITHUB_REF='refs/tags/v0.0.1'), 'exact existing tag'),
        (dict(RELEASE_TAG=website_tag, GITHUB_REF='refs/tags/' + website_tag), 'tag kind must match'),
        (dict(RELEASE_TAG='v0.0.1-hourly.2026111110',
              GITHUB_REF='refs/tags/v0.0.1-hourly.2026111110'), 'declared version'),
        (dict(GITHUB_SHA='c' * 40), 'workflow commit'),
        (dict(PUBLISH='yes'), 'true or false'),
    ):
        exercise(env=env, error=error)
    exercise('app', error='product or www')
    for fault, error in (('dirty', 'source must be clean'), ('tag', 'existing release tag'),
                         ('source', 'invalid source provenance'), ('ci', 'missing successful CI')):
        exercise(fault=fault, error=error)
    for key, error in (('WINDOWS_SIGNING_CONFIGURED', 'Windows Authenticode'),
                       ('IOS_SIGNING_CONFIGURED', 'iOS distribution identity')):
        exercise(env={key: 'false', 'RELEASE_TAG': '', 'PUBLISH': 'false'},
                 identity=dict(version='0.0.1', tag='v0.0.1', channel='stable'), error=error)
    with tempfile.TemporaryDirectory() as directory:
        output = Path(directory) / 'output'
        for key in ('app', 'version', 'tag', 'source_commit', 'channel'):
            for value in ('injected\nkey=true', 'injected\rkey=true', False):
                try:
                    write_outputs({**context, key: value, 'publish': False}, output)
                except ValueError as failure:
                    release.require('single-line identity' in str(failure), 'Wrong output rejection')
                    rejected += 1
                else:
                    raise ValueError('Unsafe workflow output was accepted')
                release.require(not output.exists(), 'Invalid identity wrote workflow outputs')
        write_outputs({**context, 'publish': False}, output)
        release.require(output.read_text() == ''.join(f'{key}={value}\n' for key, value in context.items())
                        + 'publish=false\n', 'Entry output differs from the exact identity')
        accepted += 1
    print(f'Release entry self-test passed: {accepted} accepted, {rejected} rejected; '
          'fixed scope, exact tag/source, source provenance, CI, signing and output integrity')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    command = parser.add_mutually_exclusive_group(required=True)
    command.add_argument('--app', choices=('product', 'www'))
    command.add_argument('--self-test', action='store_true')
    args = parser.parse_args()
    if args.self_test:
        self_test()
    else:
        write_outputs(resolve_entry(args.app), os.environ['GITHUB_OUTPUT'])


if __name__ == '__main__':
    main()
