"""Single-attempt GitHub commands and response bytes; no publication policy."""
import json
from pathlib import Path
import subprocess
import release_archive
import release_context
import release_format


def github_json(path):
    return json.loads(release_context.run('gh', 'api', f'repos/{release_format.GITHUB_REPO}/{path}'))


def fetch_source(tag):
    args = ['git', 'fetch', '--quiet', '--tags']
    if release_context.run('git', 'rev-parse', '--is-shallow-repository') == 'true':
        args.append('--unshallow')
    subprocess.run([*args, 'origin', 'main', 'refs/tags/' + tag], cwd=release_context.ROOT, check=True)
    return release_context.run('git', 'rev-parse', tag + '^{commit}')


def download_assets(tag, selected, output):
    output = Path(output)
    output.mkdir(parents=True, exist_ok=True)
    for asset in selected:
        destination = output / asset['name']
        if not destination.exists():
            subprocess.run(['gh', 'release', 'download', tag, '--repo', release_format.GITHUB_REPO, '--pattern', asset['name'], '--dir', str(output)], check=True)
        release_format.require(destination.is_file() and not destination.is_symlink() and destination.stat().st_size == asset['size']
                and release_archive.digest(destination) == asset['sha256'], 'downloaded dependency bytes differ')
    return output


def ci_runs(source):
    return json.loads(release_context.run('gh', 'api', '--paginate', '--slurp',
        f'repos/{release_format.GITHUB_REPO}/actions/workflows/ci.yml/runs?head_sha={source}&branch=main&event=push&per_page=100'))


def asset_bytes(asset_id):
    return subprocess.check_output(['gh', 'api', '-H', 'Accept: application/octet-stream',
        f'repos/{release_format.GITHUB_REPO}/releases/assets/{asset_id}'], cwd=release_context.ROOT)


def release_view(tag, repo):
    return subprocess.run(['gh', 'release', 'view', tag, '--repo', repo, '--json', 'isDraft,assets'],
                          capture_output=True, text=True)


def release_create(tag, repo, prerelease):
    """Attempt one create. An unknown response is left for the caller to reconcile."""
    subprocess.run(['gh', 'release', 'create', tag, '--repo', repo, '--verify-tag', '--title', tag,
                    '--notes', 'BUILDING; incomplete until release.json is published.', '--latest=false',
                    '--prerelease=' + str(prerelease).lower()], check=True)


def release_upload(tag, repo, path):
    """Attempt one upload without clobber or retry."""
    subprocess.run(['gh', 'release', 'upload', tag, '--repo', repo, str(path)], check=True)


def release_edit(tag, repo, notes):
    """Attempt one final notes edit without retry."""
    subprocess.run(['gh', 'release', 'edit', tag, '--repo', repo, '--notes', notes, '--latest=false'], check=True)
