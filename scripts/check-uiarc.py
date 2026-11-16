#!/usr/bin/env python3
"""Check Arc source integrity, route coverage and the single-system UI policy."""

import hashlib
import json
import re
import runpy
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
IMPORT = re.compile(r'(?:from\s*|import\s*)["\']([^"\']+)["\']')
RULES = {
    'legacy component import': re.compile(r'(?:@/|#)components/ui/|@shadcn/|["\']radix-ui["\']|@rn-primitives/'),
    'legacy theme role': re.compile(
        r'\b(?:bg|from|via|to)-(?:card|popover|primary|secondary|muted|brand|sidebar)(?![\w-])'
        r'|\b(?:text|border|ring|outline|divide|fill|stroke)-(?:card-foreground|popover-foreground|primary(?:-foreground)?|secondary-foreground|muted-foreground|brand(?:-foreground|-soft)?|destructive|input|sidebar(?:-border|-foreground|-accent)?)(?![\w-])'
        r'|var\(--(?:card|card-foreground|popover|popover-foreground|primary|primary-foreground|secondary|secondary-foreground|muted|muted-foreground|brand|brand-soft|destructive|input|ring|sidebar(?:-[\w-]+)?)\)'
    ),
    'legacy type scale': re.compile(r'\btext-(?:title|heading|body|meta)(?![\w-])'),
    'legacy corner scale': re.compile(r'\brounded-(?:sm|md|lg|xl|[2-4]xl)(?![\w-])'),
    'unsupported font weight': re.compile(r'\bfont-(?:semibold|bold|extrabold|black)\b|font-weight\s*:\s*(?:[6-9]00|bold)\b|fontWeight\s*:\s*["\']?[6-9]00'),
}
FORBIDDEN_PACKAGES = re.compile(r'^(?:shadcn|@shadcn/|radix-ui$|class-variance-authority$|cn$|tw-animate-css$|@rn-primitives/|react-native-reusables|@mui/|@chakra-ui/|antd$|native-base$|react-native-paper$|tamagui$|@tamagui/|@heroui/|@nextui-org/)')


def source_files(directory):
    return sorted(path for path in directory.rglob('*') if path.suffix in {'.ts', '.tsx', '.css'}
                  and not {'node_modules', 'generated', 'dist', '.expo'}.intersection(path.parts)
                  and not path.name.endswith(('.test.ts', '.d.ts', '.gen.ts')))


def source_integrity(errors):
    directory = ROOT / 'packages/uiarc'
    manifest = json.loads((directory / 'source.json').read_text())
    for name, origin in manifest['files'].items():
        path = directory / 'src' / name
        if not path.is_file() or hashlib.sha256(path.read_bytes()).hexdigest() != origin['sha256']:
            errors.append(f'{path.relative_to(ROOT)}: pinned Arc source differs from {manifest["commit"]}')
    for path in source_files(directory / 'src'):
        if path.relative_to(directory / 'src').as_posix() not in manifest['files']:
            errors.append(f'{path.relative_to(ROOT)}: source is missing from the provenance manifest')
    module = runpy.run_path(str(ROOT / 'scripts/generate-uiarc-theme.py'))
    for path, expected in module['outputs']().items():
        if not path.is_file() or path.read_text() != expected:
            errors.append(f'{path.relative_to(ROOT)}: stale or hand-edited generated Arc tokens')
    return manifest


def resolve_import(specifier, importer, app):
    if specifier.startswith('@/'):
        candidate = app / 'src' / specifier[2:]
    elif specifier.startswith('#components/'):
        candidate = app / 'src/components' / specifier.removeprefix('#components/')
    elif specifier.startswith('.'):
        candidate = importer.parent / specifier
    else:
        return None
    for path in (candidate, Path(str(candidate) + '.ts'), Path(str(candidate) + '.tsx'), candidate / 'index.ts', candidate / 'index.tsx'):
        if path.is_file():
            return path.resolve()
    return None


def reaches_arc(path, app, visiting):
    if path in visiting:
        return False
    visiting = visiting | {path}
    text = path.read_text()
    if '@vibeface/uiarc/' in text or (app.name == 'mobile' and path.is_relative_to(app / 'src/components/arc')):
        return True
    for specifier in IMPORT.findall(text):
        target = resolve_import(specifier, path, app)
        if target is not None and target.suffix in {'.ts', '.tsx'} and reaches_arc(target, app, visiting):
            return True
    return False


def policy(errors):
    counts = {}
    config = json.loads((ROOT / 'config/uiarc.json').read_text())
    for name in ('app', 'mobile'):
        app = (ROOT / 'apps' / name).resolve()
        for old in ('components.json', 'src/components/ui'):
            if (app / old).exists():
                errors.append(f'apps/{name}/{old}: remove the old UI layer')
        package = json.loads((app / 'package.json').read_text())
        for section in ('dependencies', 'devDependencies'):
            for dependency in package.get(section, {}):
                if FORBIDDEN_PACKAGES.match(dependency):
                    errors.append(f'apps/{name}/package.json: unsupported UI dependency {dependency}')
        for path in source_files(app):
            for number, line in enumerate(path.read_text().splitlines(), 1):
                for label, pattern in RULES.items():
                    if pattern.search(line):
                        errors.append(f'{path.relative_to(ROOT)}:{number}: {label}')
        routes = config['routes'][name]
        owners = config.get('presentationOwners', {}).get(name, {})
        for relative in routes:
            path = app / relative
            if not path.is_file():
                errors.append(f'apps/{name}/{relative}: existing route is missing')
                continue
            owner = owners.get(relative, relative)
            if owner is None:
                if re.search(r'(?:return\s*|=>\s*)<[A-Za-z]', path.read_text()):
                    errors.append(f'apps/{name}/{relative}: nonvisual route now renders UI without an Arc composition')
            elif not (app / owner).is_file() or not reaches_arc((app / owner).resolve(), app, set()):
                errors.append(f'apps/{name}/{relative}: route has no Arc component composition')
        counts[name] = len(routes)
    pinned = json.loads((ROOT / 'packages/uiarc/source.json').read_text())
    for app, key in (('app', 'compositions'), ('mobile', 'ports')):
        origins_file = ROOT / 'apps' / app / 'src/components/arc/source.json'
        if not origins_file.is_file():
            errors.append(f'{origins_file.relative_to(ROOT)}: missing Arc source mapping')
            continue
        origins = json.loads(origins_file.read_text())
        if origins.get('commit') != pinned['commit'] or origins.get('repository') != pinned['repository']:
            errors.append(f'{origins_file.relative_to(ROOT)}: origin differs from the pinned Arc library')
        mappings = origins.get(key, {})
        for name, items in mappings.items():
            if not (origins_file.parent / name).is_file() or not items or any(item not in pinned['items'] for item in items):
                errors.append(f'{origins_file.relative_to(ROOT)}: invalid Arc source mapping {name}')
        for path in source_files(origins_file.parent):
            if path.relative_to(origins_file.parent).as_posix() not in mappings:
                errors.append(f'{path.relative_to(ROOT)}: composition is missing its Arc source mapping')
    return counts


def main():
    errors = []
    manifest = source_integrity(errors)
    counts = policy(errors)
    if errors:
        print('Arc UI check failed:')
        for error in errors:
            print('  ' + error)
        print(f'{len(errors)} violations')
        return 1
    print(f'Arc UI verified: {len(manifest["items"])} pinned items, {len(manifest["files"])} original source files, '
          f'{counts["app"]} Web routes, {counts["mobile"]} native routes, no legacy UI system')
    return 0


if __name__ == '__main__':
    sys.exit(main())
