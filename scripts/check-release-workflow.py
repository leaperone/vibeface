#!/usr/bin/env python3
"""Check the release DAG and evaluate its real collection/publication guards."""
import argparse
import ast
from pathlib import Path
import re
from unittest import TestCase

ROOT = Path(__file__).resolve().parent.parent
TARGETS = ('macos', 'linux-x64', 'linux-arm64', 'windows')
DESKTOP_TARGETS = TARGETS


def require(condition, message):
    if not condition:
        raise ValueError(message)


def jobs(source):
    require('\njobs:\n' in source, 'workflow has no jobs')
    body = source.split('\njobs:\n', 1)[1]
    matches = list(re.finditer(r'^  ([a-z][a-z0-9-]*):\n', body, re.M))
    return {match[1]: body[match.end():matches[index + 1].start() if index + 1 < len(matches) else len(body)]
            for index, match in enumerate(matches)}


def needs(block):
    match = re.search(r'^    needs: \[([^\]\n]+)\]$', block, re.M)
    require(match is not None, 'use explicit needs lists for the release DAG')
    return set(value.strip() for value in match[1].split(','))


def guard(block):
    match = re.search(r'^    if: (.*)$', block, re.M)
    require(match is not None, 'release gate has no condition')
    if match[1] != '>-':
        return match[1]
    continuation = block[match.end() + 1:]
    lines = []
    for line in continuation.splitlines():
        if line and not line.startswith('      '):
            break
        if line.strip():
            lines.append(line.strip())
    return ' '.join(lines)


def evaluate(expression, results, scope='product', publishing=True, cancelled=False):
    expression = expression.replace('always()', 'True').replace('cancelled()', repr(cancelled))
    expression = expression.replace('needs.identity.outputs.app', repr(scope))
    expression = expression.replace('needs.identity.outputs.publish', repr(str(publishing).lower()))
    expression = re.sub(r'needs\.([a-z][a-z0-9-]*)\.result', lambda match: repr(results[match[1]]), expression)
    expression = expression.replace('&&', ' and ').replace('||', ' or ').replace('!', ' not ').strip()
    tree = ast.parse(expression, mode='eval')
    allowed = (ast.Expression, ast.BoolOp, ast.And, ast.Or, ast.UnaryOp, ast.Not,
               ast.Compare, ast.Eq, ast.Constant, ast.Load)
    require(all(isinstance(node, allowed) for node in ast.walk(tree)), 'unsupported release guard expression')
    return eval(compile(tree, '<release guard>', 'eval'), {'__builtins__': {}}, {})


PARTS = {
    'publish-app': 'static',
    'publish-native-macos': 'compose-macos',
    'publish-native-linux-x64': 'compose-linux-x64',
    'publish-native-linux-arm64': 'compose-linux-arm64',
    'publish-native-windows': 'compose-windows',
    'publish-desktop-macos': 'desktop-macos',
    'publish-desktop-linux-x64': 'desktop-linux-x64',
    'publish-desktop-linux-arm64': 'desktop-linux-arm64',
    'publish-desktop-windows': 'desktop-windows',
    'publish-android': 'android',
    'publish-ios': 'ios',
}


def check(source, helpers):
    workflow = jobs(source)
    allowed_auto = {'deploy-published-product'}
    require(not any('deploy' in name or 'dispatches' in block or 'scripts/deploy.py' in block or 'scripts/deploy-run.py' in block
                    or './.github/workflows/deploy-' in block
                    for name, block in workflow.items() if name not in allowed_auto),
            'artifact publication must not deploy services before its complete-release gate')
    require('release-${{ inputs.tag || github.ref_name }}-' in source,
            'tag pushes and manual publication must share the same concurrency group')
    identity = workflow['identity']
    require('scripts/release-entry.py --app product' in identity
            and "tags: ['v*']" in source and 'inputs.app' not in source
            and 'www-v' not in source and 'publish' not in workflow,
            'the product entry must have a fixed identity and no website selector or publisher')
    require('scripts/release.py plan --app product --tag "$RELEASE_TAG" --require-main --publish' in helpers['npm-publish.yml'],
            'npm publication must enforce the same release source rule')
    producers = ['static', 'android', 'ios', *('native-' + target for target in TARGETS)]
    for producer in producers:
        require(needs(workflow[producer]) == {'identity'}, producer + ' waits on an unrelated build')
    for target in TARGETS:
        require(needs(workflow['compose-' + target]) == {'identity', 'static', 'native-' + target},
                target + ' packaging waits on another platform')
    for target in DESKTOP_TARGETS:
        require(needs(workflow['desktop-' + target]) == {'identity', 'compose-' + target},
                target + ' desktop packaging waits on another platform')
    collection_needs = {'identity', 'static', 'android', 'ios', *('compose-' + target for target in TARGETS),
                        *('desktop-' + target for target in DESKTOP_TARGETS)}
    require(needs(workflow['collect']) == collection_needs, 'collection is missing a required producer')
    require(needs(workflow['release-start']) == {'identity'}, 'Release creator must only wait for identity')
    creator = workflow['release-start']
    require("needs.identity.outputs.publish == 'true'" in guard(creator),
            'Release creator must be limited to publishing product runs')
    require('contents: write' in creator and 'scripts/release.py begin-product' in creator,
            'Release creator must create the product Release with write permission')
    require("ref: '${{ needs.identity.outputs.tag }}'" in creator and 'fetch-depth: 0' in creator,
            'Release creator must check out the exact fully fetched release tag')

    require(set(name for name in workflow if name.startswith('publish-') and name != 'publish-product') == set(PARTS),
            'workflow must define exactly the eleven product part publishers')
    for name, producer in PARTS.items():
        block = workflow[name]
        require(needs(block) == {'identity', 'release-start', producer},
                name + ' must wait only for identity, the single creator and its own producer')
        expression = guard(block)
        require("needs.identity.outputs.publish == 'true'" in expression,
                name + ' must be product-publish only')
        require('contents: write' in block and 'release-publish-part.yml' in block,
                name + ' must call the isolated part publisher with write permission')
        require('scripts/release.py publish ' not in block,
                name + ' must not run the complete-release publisher')

    final_parts = set(PARTS)
    require(needs(workflow['publish-product']) == {'identity', 'collect', *final_parts},
            'final product publisher must wait for collect and every product part')
    final = workflow['publish-product']
    final_guard = guard(final)
    require('always()' in final_guard and '!cancelled()' in final_guard
            and "needs.identity.outputs.publish == 'true'" in final_guard
            and "needs.collect.result == 'success'" in final_guard,
            'final product marker must be publish-only and require the complete audit')
    for publisher in PARTS:
        require(f'needs.{publisher}.result == \'success\'' in final_guard,
                'final product marker is missing successful ' + publisher)
    require('contents: write' in final and 'scripts/release.py publish product .artifacts/release' in final,
            'final product marker must use the complete collected artifact')
    require("ref: '${{ needs.identity.outputs.tag }}'" in final and 'fetch-depth: 0' in final,
            'final product publisher must check out the exact fully fetched release tag')
    require('scripts/release.py begin-product' not in final and 'scripts/release.py publish-part' not in final,
            'final product job must only finalize the complete Release')

    marker = workflow.get('release-failed', '')
    require(needs(marker) == {'identity', 'release-start', 'publish-product'}
            and 'always()' in guard(marker)
            and "needs.identity.outputs.publish == 'true'" in guard(marker)
            and "needs.publish-product.result != 'success'" in guard(marker)
            and 'scripts/release.py fail-product' in marker and 'scripts/release.py publish' not in marker
            and 'scripts/release.py begin-product' not in marker,
            'failed product run must only mark its unfinished Release')

    for name, block in workflow.items():
        if name not in {'release-start', 'publish-product', 'release-failed', *PARTS}:
            require('contents: write' not in block and 'scripts/release.py publish' not in block
                    and 'scripts/release.py begin-product' not in block,
                    name + ' must not mutate a public Release')
    require(source.count('scripts/release.py publish ') == 1,
            'only the complete product finalizer may publish the complete product')

    part_helper = helpers['release-publish-part.yml']
    part_jobs = jobs(part_helper)
    require(set(part_jobs) == {'publish-part'} and 'contents: write' in part_helper
            and 'actions: read' in part_helper,
            'part helper must be the sole write-enabled upload-only workflow')
    require("ref: '${{ github.sha }}'" in part_helper and 'fetch-depth: 0' in part_helper,
            'part helper must check out the exact source with tags and origin/main available')
    require('artifact-ids: ${{ inputs.artifact_id }}' in part_helper
            and 'pattern:' not in part_helper and 'run-id:' not in part_helper,
            'part helper must download only its same-run immutable artifact ID')
    require('publish-part' in part_helper and 'begin-product' not in part_helper
            and 'scripts/release.py publish ' not in part_helper,
            'part helper may only append its verified release part')
    for name, helper in helpers.items():
        if name != 'release-publish-part.yml':
            require('contents: write' not in helper and 'scripts/release.py publish ' not in helper
                    and 'scripts/release.py publish-part' not in helper,
                    name + ' must only produce artifacts or consume a completed release')
    for name, content in [('release.yml', source), *helpers.items()]:
        for step in re.split(r'(?m)^      - ', content):
            if step.startswith('uses: actions/download-artifact@'):
                require('artifact-ids:' in step and 'pattern:' not in step,
                        name + ' downloads an unpinned artifact')
    good = {name: 'success' for name in collection_needs}
    collect = guard(workflow['collect'])
    require(evaluate(collect, good), 'complete product cannot be collected')
    for producer in collection_needs:
        for failure in ('failure', 'cancelled', 'skipped'):
            require(not evaluate(collect, {**good, producer: failure}),
                    producer + ' failure exposes an incomplete product')
    require(not evaluate(collect, good, cancelled=True), 'cancelled product must not be collected')
    require(not evaluate(guard(creator), {}, publishing=False),
            'Release creator must be disabled for build-only runs')
    for name, block in ((name, workflow[name]) for name in PARTS):
        require(evaluate(guard(block), {}, scope='product', publishing=True)
                and not evaluate(guard(block), {}, scope='product', publishing=False),
                name + ' must be disabled outside product publishing runs')
    final_success = {name: 'success' for name in needs(final)}
    require(evaluate(final_guard, final_success), 'complete product cannot be finalized')
    for failed_job in needs(final) - {'identity'}:
        for failure in ('failure', 'cancelled', 'skipped'):
            require(not evaluate(final_guard, {**final_success, failed_job: failure}),
                    failed_job + ' failure must block the final completion marker')
    require(not evaluate(final_guard, final_success, cancelled=True),
            'cancelled product must not receive a completion marker')

    npm = workflow['npm']
    require(needs(npm) == {'identity', 'collect', 'publish-product'}
            and "needs.publish-product.result == 'success'" in guard(npm),
            'npm publication must wait for the completed product release')
    check_auto_dispatch(workflow['deploy-published-product'], 'product')
    return workflow


def check_identity_helper(source):
    required = (
        "environ['GITHUB_REPOSITORY'] == release.GITHUB_REPO",
        "environ['GITHUB_REF'] == 'refs/tags/' + tag",
        'require_main=publish, publishing=publish',
        "result['source_commit'] == environ['GITHUB_SHA']",
        'release.require_ci(result)',
        "environ.get('WINDOWS_SIGNING_CONFIGURED') == 'true'",
        "environ.get('IOS_SIGNING_CONFIGURED') == 'true'",
    )
    require(all(fragment in source for fragment in required),
            'release entries must retain source, CI, exact tag and stable signing boundaries')


def check_website(source):
    workflow = jobs(source)
    require(set(workflow) == {'identity', 'static', 'collect', 'publish', 'deploy-published-www'},
            'website publication must have its own publication and deployment DAG')
    require("tags: ['www-v*']" in source and 'inputs.app' not in source
            and 'scripts/release-entry.py --app www' in workflow['identity'],
            'website entry must select only the independent website identity')
    auto = workflow['deploy-published-www']
    publication = source.replace('  deploy-published-www:\n' + auto, '')
    require('deploy' not in publication and 'dispatches' not in publication
            and 'scripts/deploy.py' not in publication and 'scripts/deploy-run.py' not in publication
            and './.github/workflows/deploy-' not in publication,
            'website publication must not deploy services before its complete-release gate')
    require('contents: write' not in workflow['static']
            and 'contents: write' not in workflow['collect'],
            'website production must not deploy or write a public release')
    require(needs(workflow['static']) == {'identity'}
            and needs(workflow['collect']) == {'identity', 'static'}
            and needs(workflow['publish']) == {'identity', 'collect'},
            'website publication must not wait for product producers')
    collect = guard(workflow['collect'])
    good = {'identity': 'success', 'static': 'success'}
    require(evaluate(collect, good), 'complete website cannot be collected')
    for producer in good:
        for failure in ('failure', 'cancelled', 'skipped'):
            require(not evaluate(collect, {**good, producer: failure}),
                    'website collection accepts a missing producer')
    require(not evaluate(collect, good, cancelled=True), 'cancelled website cannot be collected')
    publish = workflow['publish']
    expression = guard(publish)
    good = {'identity': 'success', 'collect': 'success'}
    require(evaluate(expression, good) and not evaluate(expression, good, publishing=False)
            and not evaluate(expression, good, cancelled=True),
            'website publication requires explicit intent and a successful complete collection')
    for producer in good:
        for failure in ('failure', 'cancelled', 'skipped'):
            require(not evaluate(expression, {**good, producer: failure}),
                    'website publication accepts an incomplete source or collection')
    require("ref: '${{ needs.identity.outputs.tag }}'" in publish and 'fetch-depth: 0' in publish
            and 'contents: write' in publish
            and 'scripts/release.py plan --app www --tag "$RELEASE_TAG" --require-main --publish' in publish
            and 'scripts/release.py publish www .artifacts/release' in publish,
            'website publisher must revalidate and publish the exact tag and verified bytes')
    for step in re.split(r'(?m)^      - ', source):
        if step.startswith('uses: actions/download-artifact@'):
            require('artifact-ids:' in step and 'pattern:' not in step and 'run-id:' not in step,
                    'website must consume its same-run immutable artifacts')
    check_auto_dispatch(workflow['deploy-published-www'], 'www')


def check_core(source):
    workflow = jobs(source)
    require(set(workflow) == {'identity', 'build', 'publish', 'failed', 'deploy'},
            'Core release must have identity, build, publish, failure and deploy jobs')
    require("tags: ['core-v*']" in source and 'scripts/core_release.py' in source,
            'Core release must use its fixed tag and verifier')
    require('actions: read' in workflow['identity']
            and 'GH_TOKEN: ${{ github.token }}' in workflow['identity'],
            'Core identity must authenticate its CI provenance query')
    for name in ('publish', 'failed'):
        require('GH_TOKEN: ${{ github.token }}' in workflow[name],
                f'Core {name} job must authenticate GitHub release writes')
    require(needs(workflow['build']) == {'identity'}
            and needs(workflow['publish']) == {'identity', 'build'}
            and needs(workflow['failed']) == {'identity', 'publish'}
            and needs(workflow['deploy']) == {'identity', 'publish'},
            'Core release dependency graph must be linear after identity')
    require("needs.identity.outputs.publish == 'true'" in guard(workflow['publish'])
            and "needs.identity.outputs.publish == 'true'" in guard(workflow['deploy'])
            and "needs.publish.result == 'success'" in guard(workflow['deploy']),
            'Core publication and deployment require explicit publish intent and success')
    require('always()' in guard(workflow['failed'])
            and "needs.publish.result != 'success'" in guard(workflow['failed'])
            and "'gh', 'release', 'edit'" in workflow['failed'],
            'Core failure job must mark an incomplete public release')
    require('cargo build --release --locked -p vibeface-core --target x86_64-unknown-linux-gnu' in workflow['build']
            and 'scripts/core_release.py package' in workflow['build']
            and 'artifact-ids: ${{ needs.build.outputs.artifact_id }}' in workflow['publish'],
            'Core release must build and publish the pinned x86_64 artifact')
    dispatch = workflow['deploy']
    require('gh workflow run deploy-node.yml' in dispatch and '--ref main' in dispatch
            and '--raw-field "tag=$CORE_TAG" --raw-field apply=true' in dispatch,
            'Core release must dispatch the Node deployment from main')
    require('gh run list' in dispatch and 'DISPATCHED_AT' in dispatch and 'deployment_run' in dispatch
            and 'gh run view "$deployment_run"' in dispatch
            and 'deployment_status' in dispatch and 'deployment_conclusion' in dispatch
            and 'Node deployment did not reach a terminal result in time' in dispatch,
            'Core release must wait for the Node deployment result')


def check_auto_dispatch(block, kind):
    publisher = 'publish-product' if kind == 'product' else 'publish'
    workflow = 'deploy-product.yml' if kind == 'product' else 'deploy-www.yml'
    require(needs(block) == {'identity', publisher},
            kind + ' deployment must wait for its identity and complete publisher')
    expression = guard(block)
    require("needs.identity.outputs.publish == 'true'" in expression
            and f"needs.{publisher}.result == 'success'" in expression,
            kind + ' deployment must require a successful real publication')
    good = {publisher: 'success'}
    require(evaluate(expression, good), kind + ' deployment cannot run after a complete publication')
    for result in ('failure', 'cancelled', 'skipped'):
        require(not evaluate(expression, {publisher: result}),
                kind + ' deployment accepts an incomplete publication')
    require('runs-on: ubuntu-24.04' in block
            and 'actions: write' in block and 'contents: read' in block
            and 'contents: write' not in block,
            kind + ' deployment must use only Actions dispatch permission')
    require('GH_TOKEN: ${{ github.token }}' in block
            and 'RELEASE_TAG: ${{ needs.identity.outputs.tag }}' in block,
            kind + ' deployment must pass the exact release tag through the token boundary')
    require(f'gh workflow run {workflow}' in block
            and '--repo "$GITHUB_REPOSITORY"' in block
            and '--ref main' in block
            and '--raw-field "tag=$RELEASE_TAG" --raw-field apply=true' in block,
            kind + ' deployment must dispatch the fixed entry from main with apply=true')
    require(block.count('gh workflow run ') == 1
            and 'scripts/deploy.py' not in block and 'scripts/deploy-run.py' not in block,
            kind + ' deployment must have one fixed dispatch command')
    require('gh run list' in block and 'DISPATCHED_AT' in block and 'deployment_run' in block
            and 'gh run view "$deployment_run"' in block
            and 'deployment_status' in block and 'deployment_conclusion' in block
            and 'deployment did not reach a terminal result in time' in block.lower(),
            kind + ' deployment must wait for the dispatched run to reach a terminal result')


def self_test(source, helpers, website, entry, core):
    check(source, helpers)
    check_website(website)
    check_core(core)
    check_identity_helper(entry)
    helper = helpers['release-publish-part.yml']
    broken = [
        source.replace('scripts/release-entry.py --app product', 'scripts/release-entry.py --app www'),
        source + '\n  deploy-app:\n    uses: ./.github/workflows/deploy-services.yml\n',
        source.replace('needs: [identity, static, native-macos]', 'needs: [identity, static, native-macos, native-linux-x64]'),
        source.replace('needs: [identity, compose-macos]', 'needs: [identity, compose-macos, compose-windows]'),
        source.replace('needs: [identity, release-start, compose-macos]',
                       'needs: [identity, release-start, compose-macos, collect]'),
        source.replace('needs: [identity, release-start, compose-macos]',
                       'needs: [identity, release-start, compose-linux-x64]'),
        source.replace('needs: [identity, release-start, static]', 'needs: [identity, static]'),
        source.replace("needs.identity.outputs.publish == 'true'", 'true', 1),
        source.replace("if: needs.identity.outputs.publish == 'true'\n    uses: ./.github/workflows/release-publish-part.yml",
                       'if: always()\n    uses: ./.github/workflows/release-publish-part.yml', 1),
        source.replace("needs.collect.result == 'success' &&\n      needs.publish-app.result == 'success'", "needs.publish-app.result == 'success'", 1),
        source.replace("needs.publish-ios.result == 'success'", 'true', 1),
        source.replace('needs: [identity, collect, publish-app, publish-native-macos, publish-native-linux-x64, publish-native-linux-arm64, publish-native-windows, publish-desktop-macos, publish-desktop-linux-x64, publish-desktop-linux-arm64, publish-desktop-windows, publish-android, publish-ios]',
                       'needs: [identity, collect, publish-app, publish-native-macos, publish-native-linux-x64, publish-native-linux-arm64, publish-native-windows, publish-desktop-macos, publish-desktop-linux-x64, publish-desktop-linux-arm64, publish-desktop-windows, publish-android]'),
        source.replace('scripts/release.py publish product .artifacts/release', 'scripts/release.py publish www .artifacts/release'),
        source.replace('scripts/release.py fail-product', 'scripts/release.py publish product .artifacts/release'),
        source.replace("always() && needs.identity.result == 'success' && needs.identity.outputs.publish == 'true' &&\n      needs.publish-product.result != 'success'", 'if: failure()'),
        source.replace("needs.publish-product.result == 'success'", 'true'),
        source.replace('gh workflow run deploy-product.yml', 'gh workflow run deploy-www.yml'),
        source.replace('--ref main', '--ref github.ref_name'),
        source.replace('actions: write', 'contents: read', 1),
    ]
    broken_helpers = [helper.replace('artifact-ids: ${{ inputs.artifact_id }}', 'pattern: release-*'),
                      helper.replace('actions: read', 'contents: read'),
                      helper.replace("fetch-depth: 0, persist-credentials: false", 'persist-credentials: false')]
    for candidate, candidate_helper in [*((item, helpers) for item in broken),
                                        *((source, {**helpers, 'release-publish-part.yml': item}) for item in broken_helpers)]:
        require(candidate != source or candidate_helper != helpers,
                'self-test mutation did not reach the workflow')
        with TestCase().assertRaises((ValueError, SyntaxError, KeyError), msg='Unsafe release dependency or gate was accepted'):
            check(candidate, candidate_helper)
    for candidate in (
        core.replace('actions: read', 'contents: read', 1),
        core.replace('GH_TOKEN: ${{ github.token }}', 'GH_TOKEN:', 1),
        core.replace('GH_TOKEN: ${{ github.token }}', 'GH_TOKEN:', 1).replace(
            'GH_TOKEN: ${{ github.token }}', 'GH_TOKEN:', 1),
    ):
        with TestCase().assertRaises(ValueError, msg='Core GitHub token boundary was removed'):
            check_core(candidate)
    for fragment in ('release.require_ci(result)', 'require_main=publish, publishing=publish',
                     "environ['GITHUB_REPOSITORY'] == release.GITHUB_REPO",
                     "environ['GITHUB_REF'] == 'refs/tags/' + tag",
                     "result['source_commit'] == environ['GITHUB_SHA']",
                     "environ.get('WINDOWS_SIGNING_CONFIGURED') == 'true'",
                     "environ.get('IOS_SIGNING_CONFIGURED') == 'true'"):
        require(fragment in entry, 'identity mutation did not reach the helper')
        with TestCase().assertRaises(ValueError, msg='Unsafe release identity was accepted'):
            check_identity_helper(entry.replace(fragment, 'True'))
    for candidate in (
        website.replace('--app www', '--app product'),
        website.replace('needs: [identity, static]', 'needs: [identity]'),
        website.replace("needs.static.result == 'success'", 'true'),
        website.replace("needs.collect.result == 'success'", 'true'),
        website.replace("needs.identity.outputs.publish == 'true'", 'true'),
        website.replace('artifact-ids: ${{ needs.static.outputs.artifact_id }}', 'pattern: static-*'),
        website.replace('scripts/release.py publish www .artifacts/release',
                        'scripts/release.py publish product .artifacts/release'),
        website.replace('      - name: Collect website assets without rebuilding',
                        '      - name: Inject deployment\n        run: gh workflow run deploy-product.yml\n      - name: Collect website assets without rebuilding'),
        website.replace("needs.publish.result == 'success'", 'true'),
        website.replace('gh workflow run deploy-www.yml', 'gh workflow run deploy-product.yml'),
        website.replace('--ref main', '--ref github.ref_name'),
        website.replace('actions: write', 'contents: read', 1),
    ):
        require(candidate != website, 'website mutation did not reach the workflow')
        with TestCase().assertRaises((ValueError, KeyError), msg='Unsafe website gate was accepted'):
            check_website(candidate)


def check_deployment(source, entries):
    workflow = jobs(source)
    require('  workflow_call:' in source and 'workflow_dispatch:' not in source and '  push:' not in source,
            'deployment implementation must only be called by a purpose-specific entry')
    require('contents: write' not in source and 'repository_dispatch' not in source,
            'deployment must not publish or dispatch another repository')
    require('group: vibeface-official-deployments' in source and 'cancel-in-progress: false' in source
            and 'fail-fast: true' in source and 'max-parallel: 1' in source,
            'shared deployment state must retain its single global lock and fail-closed serial service matrix')
    require(set(entries) == {'node', 'app', 'www', 'all'}, 'missing a fixed deployment entry')
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
    for branch in ('refs/heads/main', 'refs/heads/feature', 'refs/tags/v0.0.1'):
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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--self-test', action='store_true')
    args = parser.parse_args()
    directory = ROOT / '.github/workflows'
    source = (directory / 'release.yml').read_text()
    helpers = {name: (directory / name).read_text()
               for name in ('release-native.yml', 'release-compose.yml', 'release-desktop.yml', 'release-publish-part.yml',
                            'release-apps.yml', 'release-ios.yml', 'npm-publish.yml', 'deploy-services.yml')}
    website = (directory / 'release-www.yml').read_text()
    core = (directory / 'release-core.yml').read_text()
    entry = (ROOT / 'scripts/release-entry.py').read_text()
    if args.self_test:
        self_test(source, helpers, website, entry, core)
        import runpy
        runpy.run_path(str(ROOT / 'scripts/release-entry.py'))['self_test']()
    else:
        check(source, helpers)
        check_website(website)
        check_core(core)
        check_identity_helper(entry)
    deployment = helpers['deploy-services.yml']
    entries = {service: (directory / f'deploy-{filename}.yml').read_text()
               for service, filename in (('node', 'node'), ('app', 'app'), ('www', 'www'), ('all', 'product'))}
    check_deployment(deployment, entries)
    if args.self_test:
        for candidate in (deployment.replace("github.ref == 'refs/heads/main' || !inputs.apply", 'true'),
                          deployment.replace('fail-fast: true', 'fail-fast: false'),
                          deployment.replace('if: inputs.apply', 'if: always()'),
                          deployment.replace('deploy-run.py rehearse', 'deploy-run.py self-test'),
                          deployment.replace("if: matrix.service == 'node'", "if: matrix.service == 'node' && inputs.apply"),
                          deployment.replace("if: matrix.service == 'node'",
                                             "if: matrix.service == 'node'\n        continue-on-error: true")):
            with TestCase().assertRaises(ValueError, msg='unsafe deployment gate was accepted'):
                check_deployment(candidate, entries)
        for service, source in entries.items():
            for candidate in (source.replace('default: false', 'default: true'),
                              source.replace(f'service: {service}', 'service: invalid'),
                              source + '\nconcurrency:\n  group: vibeface-official-deployments\n'):
                with TestCase().assertRaises(ValueError, msg='unsafe fixed deployment entry was accepted'):
                    check_deployment(deployment, {**entries, service: candidate})
    print('Release DAG passed: parallel native, desktop, Web and Android producers, per-platform composition, '
          'exact artifacts, progressive uploads and complete-only publication with automatic deployment dispatch')


if __name__ == '__main__':
    main()
