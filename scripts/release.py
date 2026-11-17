#!/usr/bin/env python3
'Independent application release identities, immutable artifacts and dependency pins.'
import argparse
from pathlib import Path
import sys
sys.dont_write_bytecode = True
import release_format
import release_context
import release_archive
import release_inventory
import release_assembly
import release_publisher


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='command', required=True)
    p = sub.add_parser('plan')
    p.add_argument('--app', choices=release_format.APPS, required=True)
    p.add_argument('--tag')
    p.add_argument('--require-main', action='store_true')
    p.add_argument('--publish', action='store_true')
    p.add_argument('--github-output')
    p = sub.add_parser('component')
    p.add_argument('app', choices=release_format.COMPONENTS)
    p.add_argument('--input', type=Path, required=True)
    p.add_argument('--output', type=Path, required=True)
    p.add_argument('--target')
    p = sub.add_parser('stage-native')
    p.add_argument('--app', choices=('cli',), default='cli')
    p.add_argument('--target', required=True)
    p.add_argument('--input', type=Path, required=True)
    p.add_argument('--output', type=Path, required=True)
    p = sub.add_parser('compose')
    p.add_argument('--target', required=True)
    p.add_argument('--native-dir', type=Path, required=True)
    p.add_argument('--app-archive', type=Path, required=True)
    p.add_argument('--core-archive', type=Path, required=True)
    p.add_argument('--output', type=Path, required=True)
    sub.add_parser('seal-bundle').add_argument('directory', type=Path)
    p = sub.add_parser('manifest')
    p.add_argument('app', choices=release_format.APPS)
    p.add_argument('directory', type=Path)
    for command in ('publish', 'verify-public'):
        p = sub.add_parser(command)
        p.add_argument('app', choices=release_format.APPS)
        p.add_argument('directory', type=Path)
    p = sub.add_parser('collect')
    p.add_argument('--app', choices=release_format.APPS, required=True)
    p.add_argument('--input-root', type=Path, required=True)
    p.add_argument('--output', type=Path, required=True)
    p = sub.add_parser('dependency')
    p.add_argument('--app', choices=release_format.APPS, required=True)
    p.add_argument('--dependency', choices=release_format.APPS, required=True)
    p.add_argument('--target')
    p.add_argument('--output', type=Path, required=True)
    p = sub.add_parser('verify-desktop')
    p.add_argument('--target', required=True)
    p.add_argument('--runtime-archive', type=Path)
    p.add_argument('directory', type=Path)
    sub.add_parser('mobile-provenance').add_argument('apk', type=Path)
    sub.add_parser('self-test')
    args = parser.parse_args()
    if args.command == 'plan':
        release_publisher.plan(args.app, args.tag, args.require_main, args.publish, args.github_output)
    elif args.command == 'component':
        release_assembly.pack_component(args.app, args.input, args.output, args.target)
    elif args.command == 'stage-native':
        release_assembly.stage_native(args.target, args.input, args.output, args.app)
    elif args.command == 'compose':
        release_assembly.compose_offline(args.target, args.native_dir, args.app_archive, args.output, args.core_archive)
    elif args.command == 'seal-bundle':
        release_archive.seal_bundle(args.directory)
    elif args.command == 'manifest':
        release_inventory.manifest(args.app, args.directory)
    elif args.command == 'collect':
        release_inventory.collect(args.app, args.input_root, args.output)
    elif args.command == 'dependency':
        release_publisher.dependency(args.app, args.dependency, args.target, args.output)
    elif args.command == 'publish':
        release_publisher.publish(args.app, args.directory)
    elif args.command == 'verify-public':
        release_publisher.verify_public(args.app, args.directory)
    elif args.command == 'mobile-provenance':
        release_archive.write_json(args.apk.with_name(args.apk.name + '.provenance.json'), release_archive.mobile_provenance(args.apk))
    elif args.command == 'verify-desktop':
        pins = {key: value for key, value in release_context.identity('desktop').items() if key != 'app'}
        release_archive.verify_desktop_receipt(args.directory, args.target, pins, release_archive.digest(args.runtime_archive) if args.runtime_archive else None)
    else:
        from tests.release.runner import self_test
        self_test()


if __name__ == '__main__':
    main()
