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


def check(source, helpers):
    workflow = jobs(source)
    require(not any('deploy' in name or 'dispatches' in block or 'scripts/deploy.py' in block
                    for name, block in workflow.items()), 'artifact publication must not deploy services')
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
    require(needs(workflow['publish']) == {'identity', 'collect'}, 'publisher must consume the complete collection')
    require('contents: write' in workflow['publish'], 'publisher needs explicit write permission')
    for name, block in workflow.items():
        if name != 'publish':
            require('contents: write' not in block and 'scripts/release.py publish ' not in block,
                    name + ' may publish a partial release')
    require(source.count('scripts/release.py publish ') == 1, 'only one GitHub Release publisher is allowed')
    for name, helper in helpers.items():
        require('contents: write' not in helper and 'scripts/release.py publish ' not in helper,
                name + ' must only produce artifacts')
    for name, content in [('release.yml', source), *helpers.items()]:
        for step in re.split(r'(?m)^      - ', content):
            if step.startswith('uses: actions/download-artifact@'):
                require('artifact-ids:' in step and 'pattern:' not in step,
                        name + ' downloads an unpinned artifact')
    good = {name: 'success' for name in collection_needs}
    collect = guard(workflow['collect'])
    publish = guard(workflow['publish'])
    require(evaluate(collect, good), 'complete product cannot be collected')
    for producer in collection_needs:
        for failure in ('failure', 'cancelled', 'skipped'):
            require(not evaluate(collect, {**good, producer: failure}),
                    producer + ' failure exposes an incomplete product')
    require(not evaluate(collect, good, cancelled=True), 'cancelled product must not be collected')
    portal = {name: ('success' if name in ('identity', 'static') else 'skipped') for name in collection_needs}
    require(evaluate(collect, portal, scope='www'), 'website must be independently releasable')
    require(evaluate(publish, {'collect': 'success'}, publishing=True)
            and not evaluate(publish, {'collect': 'success'}, publishing=False),
            'build-only runs may publish publicly')
    for failure in ('failure', 'cancelled', 'skipped'):
        require(not evaluate(publish, {'collect': failure}), 'incomplete collection may publish publicly')
    return workflow


def self_test(source, helpers):
    check(source, helpers)
    broken = [
        source + '\n  deploy-app:\n    uses: ./.github/workflows/deploy-services.yml\n',
        source.replace('needs: [identity, static, native-macos]', 'needs: [identity, static, native-macos, native-linux-x64]'),
        source.replace('needs: [identity, compose-macos]', 'needs: [identity, compose-macos, compose-windows]'),
        source.replace("needs.android.result == 'success'", "True"),
        source.replace("needs.ios.result == 'success' &&", ''),
        source.replace("needs.desktop-windows.result == 'success' &&", ''),
        source.replace("needs.desktop-linux-x64.result == 'success' &&", ''),
        source.replace("needs.desktop-linux-arm64.result == 'success' &&", ''),
        source.replace("needs.compose-windows.result == 'success' &&", ''),
        source.replace("if: needs.identity.outputs.publish == 'true' && needs.collect.result == 'success'", 'if: always()'),
        source.replace("&& needs.collect.result == 'success'", ''),
        source.replace('artifact-ids: ${{ needs.static.outputs.artifact_id }}', 'pattern: static-*'),
    ]
    for candidate in broken:
        require(candidate != source, 'self-test mutation did not reach the workflow')
        try:
            check(candidate, helpers)
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
               for name in ('release-native.yml', 'release-compose.yml', 'release-desktop.yml',
                            'release-apps.yml', 'release-ios.yml', 'npm-publish.yml', 'deploy-services.yml', 'deploy-www.yml')}
    self_test(source, helpers) if args.self_test else check(source, helpers)
    print('Release DAG passed: parallel native, desktop, Web and Android producers, per-platform composition, '
          'exact artifacts and complete-only publication')


if __name__ == '__main__':
    main()
