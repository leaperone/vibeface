#!/usr/bin/env python3
"""Generate nginx configuration from a verified, published static component."""
import argparse
import json
from pathlib import Path
import sys

from runtime import component_info, regular, require


def quoted(value):
    require(isinstance(value, str) and value and not any(c in value for c in '\r\n\x00'), 'invalid nginx value')
    return '"' + value.replace('\\', '\\\\').replace('"', '\\"').replace('$', '\\$') + '"'


def headers(path):
    sections = {}
    current = None
    for line in regular(path, 65536).decode().splitlines():
        if line.startswith('  '):
            require(current is not None and ': ' in line, 'invalid published static header')
            name, value = line.strip().split(': ', 1)
            require(name in ('Content-Security-Policy', 'X-Content-Type-Options', 'X-Frame-Options',
                             'Referrer-Policy', 'Cache-Control') and name not in sections[current],
                    'unsupported or duplicate published static header')
            sections[current][name] = value
        else:
            require(line and line not in sections, 'invalid published header path')
            current = line
            sections[current] = {}
    require(set(sections.get('/*', {})) == {'Content-Security-Policy', 'X-Content-Type-Options',
                                           'X-Frame-Options', 'Referrer-Policy'}, 'missing security headers')
    require(sections.get('/assets/*') == {'Cache-Control': 'public, max-age=31536000, immutable'},
            'unsupported immutable asset policy')
    for pattern, values in sections.items():
        if pattern != '/*':
            require(set(values) == {'Cache-Control'} and values['Cache-Control'] in
                    ('no-store', 'no-cache', 'public, max-age=31536000, immutable'), 'unsupported cache policy')
    return sections


def generate(component, role):
    info = component_info(component)
    require(info['app'] == role and role in ('app', 'www'), 'wrong static component role')
    security = {}
    cache = ['    default "no-store";']
    if role == 'app':
        web = component / 'web'
        sections = headers(web / '_headers')
        receipt = json.loads(regular(web / '.vibeface-build.json'))
        require(receipt['version'] == 1 and receipt['component'] == 'web'
                and sections['/*']['Content-Security-Policy'] == receipt['content_security_policy'],
                'published App CSP differs from its build receipt')
        require(all((web / name).is_file() for name in ('_shell.html', 'index.html',
                'vibeface_secure.js', 'vibeface_secure_bg.wasm')), 'published App files are incomplete')
        security = sections.pop('/*')
        for pattern, values in sections.items():
            require(pattern.startswith('/') and (pattern.count('*') == 0 or pattern == '/assets/*'),
                    'unsupported published cache path')
            selector = '~^/assets/' if pattern == '/assets/*' else quoted(pattern)
            cache.append(f'    {selector} {quoted(values["Cache-Control"])};')
        cache.append('    ~\\.[^/]+$ "no-cache";')
    else:
        require((component / 'site/index.html').is_file(), 'published website has no index')
        cache = ['    default "no-cache";']
    security.update({'X-Vibeface-Source': info['source_commit'], 'X-Vibeface-Version': info['version']})
    header_lines = '\n'.join('        add_header ' + key + ' ' + quoted(value) + ' always;'
                             for key, value in security.items())
    fallback = 'try_files $uri /_shell.html;' if role == 'app' else 'try_files $uri $uri/ $uri/index.html =404;'
    app_routes = '''
        location = /api { return 404; }
        location ^~ /api/ { return 404; }
        location ^~ /sdk.v1. { return 404; }
        location = /assets { return 404; }
        location ^~ /assets/ { try_files $uri =404; }
        location = /generated { return 404; }
        location ^~ /generated/ { try_files $uri =404; }
        location ~ \\.[^/]+$ { try_files $uri =404; }
''' if role == 'app' else ''
    return f'''pid /tmp/nginx.pid;
error_log /dev/stderr warn;
events {{ worker_connections 1024; }}
http {{
    include /etc/nginx/mime.types;
    default_type application/octet-stream;
    access_log /dev/stdout;
    server_tokens off;
    client_body_temp_path /tmp/client_body;
    proxy_temp_path /tmp/proxy;
    fastcgi_temp_path /tmp/fastcgi;
    uwsgi_temp_path /tmp/uwsgi;
    scgi_temp_path /tmp/scgi;
    map $uri $vibeface_cache {{
{chr(10).join(cache)}
    }}
    server {{
        listen 8080;
        server_name _;
        root /usr/share/nginx/html;
        index index.html;
{header_lines}
        add_header Cache-Control $vibeface_cache always;
        if ($request_method !~ ^(GET|HEAD)$) {{ return 405; }}
        if ($request_uri ~* "%2e|%5c") {{ return 404; }}
        if ($uri ~ "(^|/)(\\\\.|_headers(?:/|$)|_redirects(?:/|$))") {{ return 404; }}
{app_routes}
        location / {{ {fallback} }}
        error_page 404 = @missing;
        location @missing {{
{header_lines}
            add_header Cache-Control "no-store" always;
            return 404;
        }}
    }}
}}
'''


def self_test():
    import hashlib
    import tempfile
    artifact_root = Path(__file__).resolve().parent.parent / '.artifacts'
    artifact_root.mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(dir=artifact_root, prefix='static-check-') as temporary:
        component = Path(temporary)
        web = component / 'web'; web.mkdir()
        sections = [('/*', [('Content-Security-Policy', "default-src 'self'"),
                             ('X-Content-Type-Options', 'nosniff'), ('X-Frame-Options', 'DENY'),
                             ('Referrer-Policy', 'no-referrer')]),
                    *[(name, [('Cache-Control', 'no-store')]) for name in ('/', '/_shell.html', '/index.html',
                       '/vibeface_secure.js', '/vibeface_secure_bg.wasm')],
                    ('/assets/*', [('Cache-Control', 'public, max-age=31536000, immutable')])]
        (web / '_headers').write_text('\n'.join(pattern + '\n' + '\n'.join('  ' + n + ': ' + v for n, v in values)
                                               for pattern, values in sections) + '\n')
        for name in ('_shell.html', 'index.html', 'vibeface_secure.js', 'vibeface_secure_bg.wasm'):
            (web / name).write_text('published fixture')
        (web / '.vibeface-build.json').write_text(json.dumps({'version': 1, 'component': 'web',
                                                            'content_security_policy': "default-src 'self'"}))
        info = {'app': 'app', 'schema': 1, 'version': '1.0.0', 'tag': 'v1.0.0', 'channel': 'stable',
                'source_commit': 'a' * 40, 'target': None,
                'files': {str(path.relative_to(component)): hashlib.sha256(path.read_bytes()).hexdigest()
                          for path in web.iterdir()}}
        (component / 'COMPONENT-INFO.json').write_text(json.dumps(info))
        result = generate(component, 'app')
        assert 'try_files $uri /_shell.html;' in result and 'location ^~ /api/ { return 404; }' in result
        assert 'X-Vibeface-Source "' + 'a' * 40 + '"' in result
        assert 'Content-Security-Policy "default-src \'self\'" always;' in result
        (web / '_shell.html').write_text('changed')
        try:
            generate(component, 'app')
        except ValueError:
            pass
        else:
            raise AssertionError('changed published component accepted')
        try:
            quoted('header\ninjection')
        except ValueError:
            pass
        else:
            raise AssertionError('nginx header injection accepted')
    print('static container configuration self-test passed')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--self-test', action='store_true')
    parser.add_argument('--component', type=Path)
    parser.add_argument('--role', choices=('app', 'www'))
    parser.add_argument('--output', type=Path)
    args = parser.parse_args()
    if args.self_test:
        self_test(); return
    require(all((args.component, args.role, args.output)), 'component, role and output are required')
    args.output.write_text(generate(args.component, args.role))


if __name__ == '__main__':
    try:
        main()
    except (ValueError, KeyError, TypeError, OSError):
        sys.exit('static configuration rejected the published component')
