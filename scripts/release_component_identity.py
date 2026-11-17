"""Pure component identity and release version facts; no filesystem or process access.

Consumers must separately enforce their target/epoch policy, pins and real file
digests. Passing this format check never proves the contents of an artifact.
"""
import datetime
import re


APPS = ('core', 'app', 'cli', 'desktop', 'mobile', 'www')
COMPONENTS = ('core', 'app', 'www')
VERSION = r'(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-(hourly)\.([0-9]{10})|-(rc)\.([1-9][0-9]*))?'
COMMIT = r'[a-f0-9]{40}'
SHA256 = r'[a-f0-9]{64}'
SCHEMA_VERSION = 1


def require(condition, message):
    if not condition:
        raise ValueError(message)


def version(value):
    require(isinstance(value, str), 'invalid release version')
    match = re.fullmatch(VERSION, value)
    require(match, 'invalid release version')
    if match[4]:
        datetime.datetime.strptime(match[5], '%Y%m%d%H')
    return 'hourly' if match[4] else 'rc' if match[6] else 'stable'


def parse_tag(tag):
    require(isinstance(tag, str), 'invalid release tag')
    match = re.fullmatch(r'(' + '|'.join(APPS) + r')-v(' + VERSION + r')', tag)
    require(match, 'tag must be <core|app|cli|desktop|mobile|www>-v<version>')
    return match[1], match[2], version(match[2])


def validate_component(info):
    """Check the closed identity format, without reading or authenticating files."""
    require(isinstance(info, dict), 'invalid component fields')
    app = info.get('app')
    fields = {'app', 'version', 'tag', 'source_commit', 'channel', 'schema', 'target', 'files'}
    require(app in COMPONENTS and set(info) == fields | ({'data_epoch'} if app == 'core' else set()),
            'invalid component fields')
    require(type(info['schema']) is int and info['schema'] == SCHEMA_VERSION, 'invalid component schema')
    require(parse_tag(info['tag']) == (app, info['version'], info['channel']), 'invalid component tag')
    require(isinstance(info['source_commit'], str) and re.fullmatch(COMMIT, info['source_commit']),
            'invalid component source')
    if app == 'core':
        require(isinstance(info['target'], str) and type(info['data_epoch']) is int,
                'invalid Core target or data epoch')
    else:
        require(info['target'] is None, 'static component has a native target')
    declared = info['files']
    require(isinstance(declared, dict) and declared, 'component has no files')
    require(all(isinstance(name, str) and name and isinstance(digest, str) and re.fullmatch(SHA256, digest)
                for name, digest in declared.items()), 'invalid component file identity')
    return info
