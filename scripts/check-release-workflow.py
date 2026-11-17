#!/usr/bin/env python3
"""Validate independent release DAGs and exercise their publication guards."""

import sys
sys.dont_write_bytecode = True
import argparse
import ast
from pathlib import Path
import re

ROOT = Path(__file__).resolve().parent.parent
ENTRIES = {'core': 'release-core.yml', 'app': 'release-app.yml', 'cli': 'release.yml',
           'desktop': 'release-desktop-app.yml', 'mobile': 'release-mobile.yml', 'www': 'release-www.yml'}
TARGETS = {'macos': 'aarch64-apple-darwin', 'linux-x64': 'x86_64-unknown-linux-gnu',
           'linux-arm64': 'aarch64-unknown-linux-gnu', 'windows': 'x86_64-pc-windows-msvc'}
HELPERS = ('release-native.yml', 'release-compose.yml', 'release-desktop.yml', 'release-apps.yml',
           'release-ios.yml', 'release-collect.yml', 'release-publish.yml', 'release-deploy.yml', 'npm-publish.yml')


def require(condition, message):
    if not condition:
        raise ValueError(message)


def jobs(source):
    require('\njobs:\n' in source, 'workflow has no jobs')
    body = source.split('\njobs:\n', 1)[1]
    matches = list(re.finditer(r'^  ([a-z][a-z0-9-]*):\n', body, re.M))
    require(len({match[1] for match in matches}) == len(matches), 'duplicate release job')
    return {match[1]: body[match.end():matches[index + 1].start() if index + 1 < len(matches) else len(body)]
            for index, match in enumerate(matches)}


def needs(block):
    match = re.search(r'^    needs: (.+)$', block, re.M)
    if not match:
        return set()
    value = match[1]
    return set(item.strip() for item in value.strip('[]').split(','))


def scalar(block, name, indent=6):
    match = re.search(r'^' + ' ' * indent + re.escape(name) + r': (.+)$', block, re.M)
    require(match is not None, 'missing release input/output ' + name)
    return match[1].strip("'\"")


def guard(block):
    match = re.search(r'^    if: (.*)$', block, re.M)
    if not match:
        return ''
    if match[1] not in ('>-', '|'):
        return match[1].strip()
    lines = []
    for line in block[match.end() + 1:].splitlines():
        if line and not line.startswith('      '):
            break
        lines.append(line.strip())
    return ' '.join(lines)


def evaluate(expression, results=None, publishing=True, cancelled=False, context=None):
    results = results or {}
    expression = expression.removeprefix('${{').removesuffix('}}').strip()
    explicit_status = re.search(r'\b(?:always|cancelled|success|failure)\s*\(', expression)
    success = not cancelled and all(value == 'success' for value in results.values())
    if not expression:
        return success
    expression = expression.replace('always()', 'True').replace('cancelled()', repr(cancelled))
    expression = expression.replace('success()', repr(success)).replace('failure()', repr('failure' in results.values()))
    values = {'needs.identity.outputs.publish': str(publishing).lower(), **(context or {})}
    for name, value in sorted(values.items(), key=lambda pair: -len(pair[0])):
        expression = expression.replace(name, repr(value))
    expression = re.sub(r'needs\.([a-z][a-z0-9-]*)\.result', lambda m: repr(results.get(m[1], 'missing')), expression)
    expression = expression.replace('&&', ' and ').replace('||', ' or ')
    expression = re.sub(r'!(?!=)', ' not ', expression).strip()
    expression = re.sub(r'\btrue\b', 'True', expression)
    expression = re.sub(r'\bfalse\b', 'False', expression)
    tree = ast.parse(expression, mode='eval')
    allowed = (ast.Expression, ast.BoolOp, ast.And, ast.Or, ast.UnaryOp, ast.Not,
               ast.Compare, ast.Eq, ast.NotEq, ast.Constant, ast.Load)
    require(all(isinstance(node, allowed) for node in ast.walk(tree)), 'unsupported release guard expression')
    result = bool(eval(compile(tree, '<release guard>', 'eval'), {'__builtins__': {}}, {}))
    return result and (bool(explicit_status) or success)


def permission(block, name, value):
    return re.search(r'\b' + re.escape(name) + r':\s*' + re.escape(value) + r'\b', block) is not None


def inherited_permission(block, header, name, value):
    return permission(block if re.search(r'^    permissions:', block, re.M) else header, name, value)


def checked_source(block):
    require(scalar(block, 'source_sha') == '${{ needs.identity.outputs.source_sha }}', 'producer must use the validated source SHA')


def check_entry(app, source):
    header = source.split('\njobs:\n', 1)[0]
    require(f"tags: ['{app}-v*']" in header and 'workflow_dispatch:' in header
            and 'workflow_call:' not in header and 'default: false' in header,
            app + ' must expose only its own tags and explicit publication intent')
    require(f'release-{app}-${{{{ inputs.tag || github.ref_name }}}}-' in header
            and 'cancel-in-progress: false' in header and not permission(header, 'contents', 'write'),
            app + ' publication retries must serialize without global write permission')
    workflow = jobs(source)
    publisher = 'publish-' + app
    producers = ({'static'} if app in ('app', 'www') else {'android', 'ios'} if app == 'mobile'
                 else {('desktop-' if app == 'desktop' else 'native-') + target for target in TARGETS})
    composed = {'compose-' + target for target in TARGETS} if app == 'cli' else set()
    optional = ({'deploy'} if app in ('core', 'app', 'www') else set()) | ({'npm'} if app == 'cli' else set())
    require(set(workflow) == {'identity', 'collect', publisher, 'wake-consumers'} | producers | composed | optional,
            app + ' release has foreign or missing producers/consumers')
    identity = workflow['identity']
    require(not needs(identity) and f'scripts/release-entry.py --app {app}' in identity
            and 'fetch-depth: 0' in identity and permission(identity, 'actions', 'read')
            and 'GH_TOKEN: ${{ github.token }}' in identity,
            app + ' identity must verify exact tag/source and CI with read access')
    for name, output in (('app', 'app'), ('tag', 'tag'), ('publish', 'publish'), ('source_sha', 'source_commit')):
        require(scalar(identity, name) == '${{ steps.plan.outputs.' + output + ' }}', 'release identity output differs')
    require('RELEASE_TAG: ${{ inputs.tag || (github.ref_type' in identity
            and 'PUBLISH: ${{ github.event_name == \'push\' || inputs.publish }}' in identity,
            'tag push and manual publication intent must reach the identity validator')
    for producer in producers | composed:
        block = workflow[producer]
        expected = {'identity', producer.replace('compose-', 'native-')} if producer in composed else {'identity'}
        require(needs(block) == expected and not guard(block), producer + ' must depend only on its own source/inputs')
        if producer != 'static':
            checked_source(block)
        if producer.startswith('native-'):
            require('uses: ./.github/workflows/release-native.yml' in block and scalar(block, 'app') == app,
                    'native producer must build its own application')
            require(scalar(block, 'target') == TARGETS[producer.removeprefix('native-')], 'native target differs')
        elif producer.startswith('compose-'):
            native = producer.replace('compose-', 'native-')
            require('uses: ./.github/workflows/release-compose.yml' in block
                    and scalar(block, 'native_artifact_id') == '${{ needs.' + native + '.outputs.artifact_id }}',
                    'CLI composition must consume its exact native artifact ID')
            require(inherited_permission(block, header, 'actions', 'read'), 'CLI caller must pass dependency CI read access')
        elif producer.startswith('desktop-'):
            require('uses: ./.github/workflows/release-desktop.yml' in block
                    and scalar(block, 'runtime_tag') == '${{ needs.identity.outputs.runtime_tag }}',
                    'Desktop must consume its pinned CLI runtime tag')
            require(inherited_permission(block, header, 'actions', 'read'), 'Desktop caller must pass dependency CI read access')
        elif producer in ('android', 'ios'):
            helper = 'release-apps.yml' if producer == 'android' else 'release-ios.yml'
            require('uses: ./.github/workflows/' + helper in block
                    and scalar(block, 'version') == '${{ needs.identity.outputs.version }}', 'Mobile producer identity differs')
        else:
            require(f'scripts/release.py component {app}' in block
                    and "ref: '${{ needs.identity.outputs.source_sha }}'" in block,
                    'static producer must build only its own validated source')
    inventory = composed or producers
    collect = workflow['collect']
    require(needs(collect) == {'identity', *inventory} and not guard(collect)
            and 'uses: ./.github/workflows/release-collect.yml' in collect and scalar(collect, 'app') == app,
            'collection must wait for every producer in its application only')
    checked_source(collect)
    require(inherited_permission(collect, header, 'actions', 'read'), 'collector caller must pass artifact read access')
    ids = scalar(collect, 'artifact_ids').split(',')
    require(len(ids) == len(inventory) and set(ids) == {'${{ needs.' + name + '.outputs.artifact_id }}' for name in inventory},
            'collection must pass every exact immutable producer artifact ID once')
    publish = workflow[publisher]
    require(needs(publish) == {'identity', 'collect'} and 'uses: ./.github/workflows/release-publish.yml' in publish
            and scalar(publish, 'app') == app and scalar(publish, 'tag') == '${{ needs.identity.outputs.tag }}'
            and scalar(publish, 'artifact_id') == '${{ needs.collect.outputs.artifact_id }}'
            and permission(publish, 'contents', 'write') and permission(publish, 'actions', 'read'),
            'publisher must consume the collected immutable artifact for this exact tag')
    good = {'identity': 'success', 'collect': 'success'}
    require(evaluate(guard(publish), good) and not evaluate(guard(publish), good, publishing=False)
            and not evaluate(guard(publish), good, cancelled=True), 'publication must require explicit intent and successful collection')
    for state in ('failure', 'cancelled', 'skipped', 'missing'):
        for name in good:
            require(not evaluate(guard(publish), good | {name: state}), 'incomplete release can be published')
    for name, block in workflow.items():
        if name != publisher:
            require(not permission(block, 'contents', 'write'), 'only the complete publisher may write release assets')
        if name not in {'deploy', 'wake-consumers'}:
            require(not permission(block, 'actions', 'write') and 'gh workflow run' not in block
                    and 'release-dispatch.py' not in block and 'scripts/deploy' not in block,
                    'producer/collector must not deploy or wake another release')
    wake = workflow['wake-consumers']
    require(needs(wake) == {publisher} and not guard(wake) and permission(wake, 'actions', 'write')
            and 'GH_TOKEN: ${{ github.token }}' in wake and 'gh workflow run hourly.yml' in wake,
            'consumer reconciliation must follow this application publication')
    for consumer in optional:
        block = workflow[consumer]
        required = {'identity', publisher} | ({'collect'} if consumer == 'npm' else set())
        require(needs(block) == required, 'consumer must wait for its own complete publication')
        good = {name: 'success' for name in required}
        require(evaluate(guard(block), good), 'completed publication cannot run its consumer')
        for dependency in required:
            for state in ('failure', 'cancelled', 'skipped'):
                require(not evaluate(guard(block), good | {dependency: state}),
                        'build-only or failed publication must not run consumers')
        if consumer == 'deploy':
            require('uses: ./.github/workflows/release-deploy.yml' in block and scalar(block, 'app') == app
                    and scalar(block, 'tag') == '${{ needs.identity.outputs.tag }}' and permission(block, 'actions', 'write'),
                    'only a published Core/App/www release may deploy its matching service')
            checked_source(block)
        else:
            require('uses: ./.github/workflows/npm-publish.yml' in block
                    and scalar(block, 'artifact_id') == '${{ needs.collect.outputs.npm_artifact_id }}'
                    and scalar(block, 'tag') == '${{ needs.identity.outputs.tag }}'
                    and scalar(block, 'publish') == 'true' and permission(block, 'id-token', 'write'),
                    'npm must use the CLI collector artifact and retain OIDC')


def check_helpers(helpers):
    for name, source in helpers.items():
        require('workflow_call:' in source, name + ' must remain reusable')
        if name != 'npm-publish.yml':
            require('workflow_dispatch:' not in source and '  push:' not in source, 'release helpers must not become entrypoints')
        if name != 'release-publish.yml':
            require(not permission(source, 'contents', 'write'), 'only the publisher may write releases')
        require('continue-on-error:' not in source, 'a failed release producer must fail its caller')
        if name in ('release-native.yml', 'release-compose.yml', 'release-desktop.yml', 'release-apps.yml', 'release-ios.yml', 'release-collect.yml'):
            outputs = re.findall(r'jobs\.([a-z][a-z0-9-]*)\.outputs\.artifact_id', source.split('\njobs:\n', 1)[0])
            require(len(outputs) == 1 and outputs[0] in jobs(source), 'reusable artifact output must name its real producer job')
            producer = jobs(source)[outputs[0]]
            require(scalar(producer, 'artifact_id') == '${{ steps.upload.outputs.artifact-id }}'
                    and 'id: upload\n' in producer and 'if-no-files-found: error' in producer,
                    'caller artifact ID must come from the successful upload step')
        for block in source.split('actions/download-artifact@')[1:]:
            step = block.split('      - ', 1)[0]
            require('artifact-ids:' in step and not re.search(r'^          (?:pattern|name):', step, re.M),
                    name + ' must download artifacts by immutable ID')
    collect = helpers['release-collect.yml']
    require('artifact-ids: ${{ inputs.artifact_ids }}' in collect
            and 'ref: \'${{ inputs.source_sha }}\'' in collect
            and 'scripts/release.py collect --app "$RELEASE_APP"' in collect
            and 'test "$(git rev-parse HEAD)" = "$SOURCE_SHA"' in collect,
            'collection must audit exact source/artifacts without rebuilding')
    require(permission(collect.split('\njobs:\n', 1)[0], 'actions', 'read'), 'collector helper must retain artifact read permission')
    require(scalar(jobs(collect)['collect'], 'npm_artifact_id') == '${{ steps.npm.outputs.artifact-id }}',
            'npm artifact output must name its real immutable upload')
    require("if: inputs.app == 'cli'" in collect and 'npm-package-${{ inputs.source_sha }}-${{ github.run_attempt }}' in collect,
            'npm artifacts must belong to the original CLI collector attempt')
    publish = helpers['release-publish.yml']
    require('artifact-ids: ${{ inputs.artifact_id }}' in publish and 'fetch-depth: 0' in publish
            and "ref: '${{ inputs.tag }}'" in publish and permission(publish, 'actions', 'read')
            and 'GH_TOKEN: ${{ github.token }}' in publish
            and 'scripts/release.py plan --app "$RELEASE_APP" --tag "$RELEASE_TAG" --require-main --publish' in publish
            and 'scripts/release.py publish "$RELEASE_APP" .artifacts/release' in publish,
            'publisher must recheck exact tag/CI and publish only audited bytes')
    compose = helpers['release-compose.yml']
    for app in ('core', 'app'):
        require(f'dependency --app cli --dependency {app} --target "$TARGET"' in compose,
                'CLI composition must resolve exact Core and App release pins')
    for name in ('release-compose.yml', 'release-desktop.yml'):
        require(permission(helpers[name], 'actions', 'read') and 'GH_TOKEN: ${{ github.token }}' in helpers[name],
                'dependency resolution requires its exact source CI read access')
    desktop = helpers['release-desktop.yml']
    require('dependency --app desktop --dependency cli --target "$TARGET"' in desktop
            and 'releases/desktop.json' in desktop and 'RUNTIME_TAG: ${{ inputs.runtime_tag }}' in desktop,
            'Desktop must verify the pinned CLI release before packaging')
    deploy = helpers['release-deploy.yml']
    require('scripts/release-dispatch.py --app "$RELEASE_APP" --tag "$RELEASE_TAG"' in deploy
            and permission(deploy, 'actions', 'write') and 'GH_TOKEN: ${{ github.token }}' in deploy,
            'automatic deployment must reconcile and wait for the exact dispatch')
    npm = helpers['npm-publish.yml']
    require(permission(npm, 'id-token', 'write') and permission(npm, 'actions', 'read')
            and 'artifact-ids: ${{ steps.artifact.outputs.artifact_id }}' in npm
            and 'run-id: ${{ steps.artifact.outputs.run_id }}' in npm
            and 'scripts/release.py plan --app cli --tag "$RELEASE_TAG" --require-main --publish' in npm
            and 'node scripts/publish-npm.mjs artifact' in npm and 'node scripts/publish-npm.mjs publish' in npm,
            'npm must preserve CLI provenance, original artifacts, OIDC and reconciliation')


def check_hourly(source):
    require('workflow_run:' in source and "workflows: ['CI / Core']" in source
            and 'types: [completed]' in source and 'branches: [main]' in source,
            'coordinator must observe completed main CI')
    workflow = jobs(source)
    require(set(workflow) == {'hourly'} and 'cancel-in-progress: false' in source
            and 'group: independent-hourly-releases' in source,
            'independent coordinator must keep one serial non-cancelling writer')
    block = workflow['hourly']
    require(permission(block, 'contents', 'write') and permission(block, 'actions', 'write')
            and 'GH_TOKEN: ${{ github.token }}' in block and 'fetch-depth: 0' in block
            and 'ref: main' in block and 'scripts/hourly.py' in block, 'coordinator source or permissions differ')
    for ref in ('refs/heads/main', 'refs/heads/other'):
        for event in ('schedule', 'workflow_dispatch', 'workflow_run'):
            for origin in ('push', 'pull_request'):
                for conclusion in ('success', 'failure', 'cancelled', 'skipped'):
                    context = {'github.ref': ref, 'github.event_name': event,
                               'github.event.workflow_run.event': origin, 'github.event.workflow_run.conclusion': conclusion}
                    expected = ref == 'refs/heads/main' and (event != 'workflow_run' or (origin == 'push' and conclusion == 'success'))
                    require(evaluate(guard(block), context=context) == expected,
                            'coordinator must reject failed/fork CI and non-main execution')


def check_deployment(source, entries):
    workflow = jobs(source)
    require('  workflow_call:' in source and 'workflow_dispatch:' not in source and '  push:' not in source,
            'deployment implementation must only be called by a purpose-specific entry')
    require('contents: write' not in source and 'repository_dispatch' not in source,
            'deployment must not publish or dispatch another repository')
    require('group: vibeface-official-deployments' in source and 'cancel-in-progress: false' in source
            and 'fail-fast: true' in source and 'max-parallel: 1' in source,
            'shared deployment state must retain its single global lock and fail-closed serial service matrix')
    require(set(entries) == {'node', 'app', 'www'}, 'missing a fixed deployment entry')
    for service, entry in entries.items():
        require('  workflow_dispatch:' in entry and '  workflow_call:' not in entry
                and '  push:' not in entry and 'schedule:' not in entry,
                'deployment entries must be explicitly dispatched')
        require(set(jobs(entry)) == {'deploy'}
                and 'uses: ./.github/workflows/deploy-services.yml' in entry
                and f'      service: {service}\n' in entry
                and '      tag: ${{ inputs.tag }}' in entry
                and '      apply: ${{ inputs.apply }}' in entry
                and '    secrets: inherit' in entry,
                'deployment entry must bind the exact target and pass explicit tag and intent')
        header = entry.split('\njobs:\n', 1)[0]
        require('service:' not in header and 'options:' not in header
                and 'default: false' in header and 'type: boolean' in header
                and 'run-name:' in header and 'concurrency:' not in entry
                and 'contents: write' not in entry,
                'deployment entry must not select targets, default to apply or deadlock its callee')
    gate = guard(workflow['plan'])
    for branch in ('refs/heads/main', 'refs/heads/feature', 'refs/tags/cli-v0.0.1'):
        for apply in (True, False):
            expression = gate.replace('github.ref', repr(branch)).replace('inputs.apply', repr(apply))
            require(evaluate(expression, {}) == (branch == 'refs/heads/main' or not apply),
                    'deployment activation must require main; branches may only validate')
    activate = source.split('      - name: Activate the pinned container on the configured host', 1)
    require(len(activate) == 2 and re.search(r'^        if: inputs\.apply$', activate[1], re.M),
            'production activation must be disabled for validation runs')
    require("|| 'vibeface-validation'" in source, 'validation must not use a production environment')
    rehearsal = source.split('      - name: Rehearse the Core activation on this runner', 1)
    require(len(rehearsal) == 2 and rehearsal[1].index('deploy-run.py rehearse') < rehearsal[1].index('deploy-run.py activate'),
            'the Core activation must be rehearsed before the host activation')
    step = rehearsal[1].split('      - name:', 1)[0]
    require("        if: matrix.service == 'node'\n" in step and step.count('if:') == 1
            and 'continue-on-error' not in step, 'every Core deployment must pass the rehearsal')


def upload_names(source, app):
    result = []
    for block in source.split('actions/upload-artifact@')[1:]:
        name = scalar(block.split('      - ', 1)[0], 'name', indent=10)
        for expression in ('${{ inputs.app }}', '${{ needs.identity.outputs.app }}'):
            name = name.replace(expression, app)
        for expression in ('${{ inputs.source_sha }}', '${{ needs.identity.outputs.source_sha }}'):
            name = name.replace(expression, '<source>')
        result.append(name)
    return result


def check_artifact_names(entries, helpers):
    for app, source in entries.items():
        collected = set(upload_names(helpers['release-collect.yml'], app))
        producers = upload_names(source, app)
        for name, helper in helpers.items():
            if name != 'release-collect.yml':
                producers.extend(upload_names(helper, app))
        require(not collected.intersection(producers),
                app + ' producer and collector artifact names must be distinct')


def check_all(entries, helpers, hourly, deployment, deployment_entries):
    require(set(entries) == set(ENTRIES), 'missing independent release entry')
    for app, source in entries.items():
        check_entry(app, source)
    check_helpers(helpers)
    check_artifact_names(entries, helpers)
    check_hourly(hourly)
    check_deployment(deployment, deployment_entries)


def self_test(entries, helpers, hourly, deployment, deployment_entries):
    checked = 0

    def rejects(action):
        nonlocal checked
        try:
            action()
        except (ValueError, KeyError, SyntaxError):
            checked += 1
        else:
            raise AssertionError('unsafe release workflow was accepted')

    colliding = entries['app'].replace('name: app-static-', 'name: app-release-', 1)
    require(colliding != entries['app'], 'App artifact collision mutation did not reach its producer')
    rejects(lambda: check_all(entries | {'app': colliding}, helpers, hourly, deployment, deployment_entries))
    colliding = helpers['release-collect.yml'].replace('name: ${{ inputs.app }}-release-', 'name: app-static-', 1)
    require(colliding != helpers['release-collect.yml'], 'artifact collision mutation did not reach the collector')
    rejects(lambda: check_all(entries, helpers | {'release-collect.yml': colliding}, hourly, deployment, deployment_entries))

    for app, source in entries.items():
        first = ('compose-macos' if app == 'cli' else 'native-macos' if app == 'core'
                 else 'desktop-macos' if app == 'desktop' else 'android' if app == 'mobile' else 'static')
        for before, after in (
            (f"tags: ['{app}-v*']", "tags: ['foreign-v*']"),
            (f'--app {app}', '--app unknown'),
            ('fetch-depth: 0', 'fetch-depth: 1'),
            ('if: needs.identity.outputs.publish == \'true\'', 'if: always()'),
            ('needs: [identity, collect]', 'needs: [identity]'),
            ('artifact_ids: ${{ needs.' + first + '.outputs.artifact_id }}', 'artifact_ids: arbitrary-name'),
            ('artifact_id: ${{ needs.collect.outputs.artifact_id }}', 'artifact_id: 123'),
            ('permissions: {contents: write, actions: read}', 'permissions: {contents: read, actions: read}'),
            ('  actions: read\n', '  actions: none\n'),
            ('scripts/release-entry.py --app', 'scripts/release.py publish'),
        ):
            candidate = source.replace(before, after, 1)
            require(candidate != source, 'mutation did not reach ' + app + ': ' + before)
            rejects(lambda: check_entry(app, candidate))
        if app == 'cli':
            candidate = source.replace('needs: [identity, collect, publish-cli]', 'needs: [identity, collect]')
            rejects(lambda: check_entry(app, candidate))
        if app in ('core', 'app', 'www'):
            candidate = source.replace('needs: [identity, publish-' + app + ']', 'needs: [identity]')
            rejects(lambda: check_entry(app, candidate))
    for filename, before, after in (
        ('release-collect.yml', 'artifact-ids: ${{ inputs.artifact_ids }}', 'pattern: *'),
        ('release-collect.yml', 'scripts/release.py collect', 'scripts/release.py manifest'),
        ('release-collect.yml', 'actions: read', 'actions: none'),
        ('release-collect.yml', 'jobs.collect.outputs.artifact_id', 'jobs.missing.outputs.artifact_id'),
        ('release-collect.yml', 'steps.npm.outputs.artifact-id', 'steps.upload.outputs.artifact-id'),
        ('release-publish.yml', 'artifact-ids: ${{ inputs.artifact_id }}', 'name: mutable-name'),
        ('release-publish.yml', '--require-main --publish', '--publish'),
        ('release-publish.yml', 'GH_TOKEN: ${{ github.token }}', 'GH_TOKEN: empty'),
        ('release-compose.yml', '--dependency core', '--dependency app'),
        ('release-desktop.yml', '--dependency cli', '--dependency core'),
        ('release-desktop.yml', 'actions: read', 'actions: none'),
        ('release-deploy.yml', 'scripts/release-dispatch.py', 'scripts/dispatch-without-wait.py'),
        ('npm-publish.yml', '--app cli', '--app app'),
        ('npm-publish.yml', 'id-token: write', 'id-token: none'),
        ('npm-publish.yml', 'run-id: ${{ steps.artifact.outputs.run_id }}', 'run-id: 123'),
    ):
        candidate = helpers[filename].replace(before, after, 1)
        require(candidate != helpers[filename], 'helper mutation did not reach ' + filename)
        rejects(lambda: check_helpers(helpers | {filename: candidate}))
    for before, after in (
        ("github.event.workflow_run.conclusion == 'success'", "github.event.workflow_run.conclusion != 'failure'"),
        ("github.event.workflow_run.event == 'push'", "github.event.workflow_run.event == 'pull_request'"),
        ("github.ref == 'refs/heads/main'", 'true'),
        ('cancel-in-progress: false', 'cancel-in-progress: true'),
        ('GH_TOKEN: ${{ github.token }}', 'GH_TOKEN: empty'),
    ):
        candidate = hourly.replace(before, after, 1)
        require(candidate != hourly, 'coordinator mutation did not reach its guard')
        rejects(lambda: check_hourly(candidate))
    for candidate in (
        deployment.replace("github.ref == 'refs/heads/main' || !inputs.apply", 'true'),
        deployment.replace('fail-fast: true', 'fail-fast: false'),
        deployment.replace('if: inputs.apply', 'if: always()'),
        deployment.replace('deploy-run.py rehearse', 'deploy-run.py self-test'),
        deployment.replace("if: matrix.service == 'node'", "if: matrix.service == 'node' && inputs.apply"),
    ):
        rejects(lambda: check_deployment(candidate, deployment_entries))
    for service, entry in deployment_entries.items():
        for candidate in (entry.replace('default: false', 'default: true'),
                          entry.replace('service: ' + service, 'service: invalid'),
                          entry + '\nconcurrency:\n  group: vibeface-official-deployments\n'):
            rejects(lambda: check_deployment(deployment, deployment_entries | {service: candidate}))
    print(f'Release workflow negative cases passed: {checked} invalid scopes, guards, pins, artifacts, permissions and deployment gates rejected')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--self-test', action='store_true')
    args = parser.parse_args()
    directory = ROOT / '.github/workflows'
    entries = {app: (directory / filename).read_text() for app, filename in ENTRIES.items()}
    helpers = {filename: (directory / filename).read_text() for filename in HELPERS}
    hourly = (directory / 'hourly.yml').read_text()
    deployment = (directory / 'deploy-services.yml').read_text()
    deployment_entries = {service: (directory / f'deploy-{service}.yml').read_text() for service in ('node', 'app', 'www')}
    check_all(entries, helpers, hourly, deployment, deployment_entries)
    if args.self_test:
        self_test(entries, helpers, hourly, deployment, deployment_entries)
        import runpy
        runpy.run_path(str(ROOT / 'scripts/release-entry.py'))['self_test']()
    print('Independent release DAG passed: six scopes, complete application manifests, exact artifact IDs, pinned dependencies and complete-only consumers')


if __name__ == '__main__':
    main()
