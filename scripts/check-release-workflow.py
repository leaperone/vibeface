#!/usr/bin/env python3
"""Check the release DAG and evaluate its real collection/publication guards."""
import argparse
import ast
from pathlib import Path
import re

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
    require('release-${{ inputs.tag || github.ref_name }}-' in source,
            'tag pushes and manual publication must share the same concurrency group')
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
    require("needs.identity.outputs.app == 'product'" in guard(creator)
            and "needs.identity.outputs.publish == 'true'" in guard(creator),
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
        require("needs.identity.outputs.app == 'product'" in expression
                and "needs.identity.outputs.publish == 'true'" in expression,
                name + ' must be product-publish only')
        require('contents: write' in block and 'release-publish-part.yml' in block,
                name + ' must call the isolated part publisher with write permission')
        require('scripts/release.py publish ' not in block,
                name + ' must not run the complete-release publisher')

    require(needs(workflow['publish']) == {'identity', 'collect'},
            'independent WWW publisher must consume its complete collection')
    www_guard = guard(workflow['publish'])
    require("needs.identity.outputs.app == 'www'" in www_guard
            and "needs.identity.outputs.publish == 'true'" in www_guard
            and "needs.collect.result == 'success'" in www_guard,
            'WWW publication must require a successful collection and explicit publish identity')
    require('contents: write' in workflow['publish'] and 'scripts/release.py publish "$SCOPE"' in workflow['publish'],
            'WWW publisher must retain the independent full website publication path')
    require("ref: '${{ needs.identity.outputs.tag }}'" in workflow['publish'] and 'fetch-depth: 0' in workflow['publish'],
            'WWW publisher must check out the exact fully fetched release tag')

    final_parts = set(PARTS)
    require(needs(workflow['publish-product']) == {'identity', 'collect', *final_parts},
            'final product publisher must wait for collect and every product part')
    final = workflow['publish-product']
    final_guard = guard(final)
    require('always()' in final_guard and '!cancelled()' in final_guard
            and "needs.identity.outputs.app == 'product'" in final_guard
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

    for name, block in workflow.items():
        if name not in {'release-start', 'publish', 'publish-product', *PARTS}:
            require('contents: write' not in block and 'scripts/release.py publish' not in block
                    and 'scripts/release.py begin-product' not in block,
                    name + ' must not mutate a public Release')
    require(source.count('scripts/release.py publish ') == 2,
            'only the independent WWW publisher and complete product finalizer may publish')

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
    portal = {name: ('success' if name in ('identity', 'static') else 'skipped') for name in collection_needs}
    require(evaluate(collect, portal, scope='www'), 'website must be independently releasable')
    require(evaluate(www_guard, {'collect': 'success'}, scope='www', publishing=True)
            and not evaluate(www_guard, {'collect': 'success'}, scope='product', publishing=True)
            and not evaluate(www_guard, {'collect': 'success'}, scope='www', publishing=False),
            'WWW publication must be independent from product and disabled for build-only runs')
    for failure in ('failure', 'cancelled', 'skipped'):
        require(not evaluate(www_guard, {'collect': failure}, scope='www'),
                'incomplete website collection may publish publicly')
    require(not evaluate(guard(creator), {}, scope='www')
            and not evaluate(guard(creator), {}, publishing=False),
            'Release creator must be disabled for WWW and build-only runs')
    for name, block in ((name, workflow[name]) for name in PARTS):
        require(evaluate(guard(block), {}, scope='product', publishing=True)
                and not evaluate(guard(block), {}, scope='www', publishing=True)
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
    require(needs(workflow['deploy-app']) == {'identity', 'publish-product'}
            and "needs.publish-product.result == 'success'" in guard(workflow['deploy-app']),
            'product deployment must wait for final release completion')
    require(needs(workflow['deploy-www']) == {'identity', 'publish'}
            and "needs.publish.result == 'success'" in guard(workflow['deploy-www']),
            'website deployment must wait for its completed website release')
    return workflow


def self_test(source, helpers):
    check(source, helpers)
    helper = helpers['release-publish-part.yml']
    broken = [
        source.replace('needs: [identity, static, native-macos]', 'needs: [identity, static, native-macos, native-linux-x64]'),
        source.replace('needs: [identity, compose-macos]', 'needs: [identity, compose-macos, compose-windows]'),
        source.replace('needs: [identity, release-start, compose-macos]',
                       'needs: [identity, release-start, compose-macos, collect]'),
        source.replace('needs: [identity, release-start, compose-macos]',
                       'needs: [identity, release-start, compose-linux-x64]'),
        source.replace('needs: [identity, release-start, static]', 'needs: [identity, static]'),
        source.replace("needs.identity.outputs.publish == 'true'", 'true', 1),
        source.replace("if: needs.identity.outputs.app == 'product' && needs.identity.outputs.publish == 'true'\n    uses: ./.github/workflows/release-publish-part.yml",
                       "if: needs.identity.outputs.app == 'www' && needs.identity.outputs.publish == 'true'\n    uses: ./.github/workflows/release-publish-part.yml", 1),
        source.replace("needs.collect.result == 'success' &&\n      needs.publish-app.result == 'success'", "needs.publish-app.result == 'success'", 1),
        source.replace("needs.publish-ios.result == 'success'", 'true', 1),
        source.replace('needs: [identity, collect, publish-app, publish-native-macos, publish-native-linux-x64, publish-native-linux-arm64, publish-native-windows, publish-desktop-macos, publish-desktop-linux-x64, publish-desktop-linux-arm64, publish-desktop-windows, publish-android, publish-ios]',
                       'needs: [identity, collect, publish-app, publish-native-macos, publish-native-linux-x64, publish-native-linux-arm64, publish-native-windows, publish-desktop-macos, publish-desktop-linux-x64, publish-desktop-linux-arm64, publish-desktop-windows, publish-android]'),
        source.replace('scripts/release.py publish product .artifacts/release', 'scripts/release.py publish www .artifacts/release'),
    ]
    broken_helpers = [helper.replace('artifact-ids: ${{ inputs.artifact_id }}', 'pattern: release-*'),
                      helper.replace('actions: read', 'contents: read'),
                      helper.replace("fetch-depth: 0, persist-credentials: false", 'persist-credentials: false')]
    for candidate, candidate_helper in [*((item, helpers) for item in broken),
                                        *((source, {**helpers, 'release-publish-part.yml': item}) for item in broken_helpers)]:
        require(candidate != source or candidate_helper != helpers,
                'self-test mutation did not reach the workflow')
        try:
            check(candidate, candidate_helper)
        except (ValueError, SyntaxError, KeyError):
            pass
        else:
            raise AssertionError('Unsafe release dependency or gate was accepted')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--self-test', action='store_true')
    args = parser.parse_args()
    directory = ROOT / '.github/workflows'
    source = (directory / 'release.yml').read_text()
    helpers = {name: (directory / name).read_text()
               for name in ('release-native.yml', 'release-compose.yml', 'release-desktop.yml', 'release-publish-part.yml',
                            'release-apps.yml', 'release-ios.yml', 'npm-publish.yml', 'deploy-app.yml', 'deploy-www.yml')}
    self_test(source, helpers) if args.self_test else check(source, helpers)
    print('Release DAG passed: parallel native, desktop, Web and Android producers, per-platform composition, '
          'exact artifacts, progressive uploads and complete-only automatic consumption')


if __name__ == '__main__':
    main()
