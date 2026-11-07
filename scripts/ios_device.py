"""Verify the program inside an iPhoneOS App, including each Mach-O slice."""
import re
import subprocess
from pathlib import Path


def require_device_commands(architectures, load_commands):
    if 'arm64' not in architectures.split():
        raise ValueError('iOS device executable has no arm64 architecture')
    commands = re.findall(r'Load command \d+\s+cmd LC_BUILD_VERSION(.*?)(?=Load command|\Z)', load_commands, re.S)
    platforms = [re.search(r'^\s*platform\s+(\d+)\s*$', command, re.M) for command in commands]
    if not platforms or any(platform is None or platform[1] != '2' for platform in platforms):
        raise ValueError('every Mach-O slice must target iPhoneOS, with no simulator platform')


def verify_device_executable(app, info):
    name = info.get('CFBundleExecutable')
    if not isinstance(name, str) or not re.fullmatch(r'[A-Za-z0-9_.-]+', name) or name in ('.', '..'):
        raise ValueError('iOS App has no valid executable name')
    executable = Path(app) / name
    if not executable.is_file() or executable.is_symlink():
        raise ValueError('iOS App executable must be a regular bundled file')
    require_device_commands(
        subprocess.check_output(['lipo', '-archs', str(executable)], text=True),
        subprocess.check_output(['otool', '-l', str(executable)], text=True),
    )
    return executable


def self_test():
    device = 'Load command 8\n      cmd LC_BUILD_VERSION\n  cmdsize 32\n platform 2\n    minos 18.0\n'
    simulator = device.replace('platform 2', 'platform 7')
    require_device_commands('arm64\n', device)
    for architectures, commands in [('x86_64', device), ('arm64', simulator), ('arm64', device + simulator), ('arm64', '')]:
        try:
            require_device_commands(architectures, commands)
        except ValueError:
            continue
        raise AssertionError('accepted an executable that does not target iPhoneOS')
    print('iOS executable platform checks passed')


if __name__ == '__main__':
    import sys
    if sys.argv[1:] != ['--self-test']:
        raise SystemExit('usage: python3 scripts/ios_device.py --self-test')
    self_test()
