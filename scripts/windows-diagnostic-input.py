#!/usr/bin/env python3
"""Select and verify a prior trusted Windows package for diagnostic-only acceptance."""
import argparse
import copy
import datetime
import json
import re
from pathlib import Path
import subprocess
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parent))

import release  # noqa: E402

TARGET = 'x86_64-pc-windows-msvc'
WORKFLOW_PATH = '.github/workflows/ci.yml'
PACKAGE_JOB = f'package ({TARGET})'
COMMIT = re.compile(r'[a-f0-9]{40}')
ARTIFACT_DIGEST = re.compile(r'sha256:([a-f0-9]{64})')
MAX_ID = 9_223_372_036_854_775_807


def require(condition, message):
    if not condition:
        raise ValueError(message)


def positive_id(value, label):
    require(type(value) is int and 0 < value <= MAX_ID, f'invalid {label}')
    return value


def run_id_argument(value):
    require(isinstance(value, str) and re.fullmatch(r'[1-9][0-9]{0,18}', value),
            'run ID must be a positive decimal integer')
    return positive_id(int(value), 'run ID')


def verify_harness_source(expected, actual):
    require(isinstance(expected, str) and COMMIT.fullmatch(expected),
            'invalid diagnostic harness source commit')
    require(actual == expected, 'diagnostic harness checkout differs from its receipt source')


def timestamp(value, label):
    require(isinstance(value, str), f'missing {label}')
    try:
        parsed = datetime.datetime.fromisoformat(value.replace('Z', '+00:00'))
    except ValueError as error:
        raise ValueError(f'invalid {label}') from error
    require(parsed.tzinfo is not None, f'{label} must include a timezone')
    return parsed.astimezone(datetime.timezone.utc)


def validate_selection(run_id, repository, run, jobs_page, artifacts_page, *, now=None):
    """Return only the exact package producer artifact from a trusted completed ci.yml run."""
    run_id = run_id_argument(str(run_id))
    require(isinstance(repository, str) and re.fullmatch(r'[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+', repository),
            'invalid repository identity')
    require(isinstance(run, dict), 'invalid workflow run metadata')
    require(positive_id(run.get('id'), 'workflow run ID') == run_id, 'workflow run ID mismatch')
    require(run.get('status') == 'completed' and isinstance(run.get('conclusion'), str),
            'selected workflow run is not complete')
    require(isinstance(run.get('path'), str) and run['path'].split('@', 1)[0] == WORKFLOW_PATH,
            'selected run did not come from ci.yml')
    source_sha = run.get('head_sha')
    require(isinstance(source_sha, str) and COMMIT.fullmatch(source_sha), 'invalid package source commit')
    repo = run.get('repository')
    head_repo = run.get('head_repository')
    require(isinstance(repo, dict) and repo.get('full_name') == repository,
            'selected run belongs to another repository')
    require(isinstance(head_repo, dict) and head_repo.get('full_name') == repository,
            'selected run source belongs to another repository')

    jobs = jobs_page.get('jobs') if isinstance(jobs_page, dict) else None
    require(isinstance(jobs, list) and type(jobs_page.get('total_count')) is int
            and jobs_page['total_count'] == len(jobs), 'workflow job metadata is incomplete')
    package_jobs = [job for job in jobs if isinstance(job, dict) and job.get('name') == PACKAGE_JOB]
    require(len(package_jobs) == 1, 'Windows package producer job is missing or ambiguous')
    package_job = package_jobs[0]
    require(package_job.get('status') == 'completed' and package_job.get('conclusion') == 'success',
            'Windows package producer did not succeed')
    require(package_job.get('head_sha') == source_sha, 'Windows package job source commit mismatch')

    artifacts = artifacts_page.get('artifacts') if isinstance(artifacts_page, dict) else None
    require(isinstance(artifacts, list) and type(artifacts_page.get('total_count')) is int
            and artifacts_page['total_count'] == len(artifacts), 'workflow artifact metadata is incomplete')
    artifact_name = f'node-package-{TARGET}-{source_sha}'
    matches = [artifact for artifact in artifacts
               if isinstance(artifact, dict) and artifact.get('name') == artifact_name]
    require(len(matches) == 1, 'Windows package artifact is missing or ambiguous')
    artifact = matches[0]
    workflow_run = artifact.get('workflow_run')
    require(isinstance(workflow_run, dict) and workflow_run.get('id') == run_id
            and workflow_run.get('head_sha') == source_sha,
            'Windows package artifact belongs to another run or source')
    if 'id' in repo:
        require(workflow_run.get('repository_id') == repo['id'], 'package artifact repository mismatch')
    artifact_id = positive_id(artifact.get('id'), 'package artifact ID')
    require(artifact.get('expired') is False, 'Windows package artifact has expired')
    expires_at = timestamp(artifact.get('expires_at'), 'artifact expiration')
    current = now or datetime.datetime.now(datetime.timezone.utc)
    require(current.tzinfo is not None and expires_at > current.astimezone(datetime.timezone.utc),
            'Windows package artifact has expired')
    require(type(artifact.get('size_in_bytes')) is int and artifact['size_in_bytes'] > 0,
            'Windows package artifact is empty')
    digest = artifact.get('digest')
    require(isinstance(digest, str) and ARTIFACT_DIGEST.fullmatch(digest),
            'Windows package artifact digest is missing or malformed')
    return {
        'run_id': run_id,
        'package_source_sha': source_sha,
        'package_job_name': PACKAGE_JOB,
        'artifact_id': artifact_id,
        'artifact_name': artifact_name,
        'artifact_digest': digest,
        'artifact_expires_at': expires_at.isoformat().replace('+00:00', 'Z'),
    }


def write_output(path, values):
    if path is None:
        return
    lines = []
    for key, value in values.items():
        require(re.fullmatch(r'[a-z][a-z0-9_]*', key), 'invalid workflow output key')
        text = str(value)
        require('\n' not in text and '\r' not in text, 'workflow output must be a single line')
        lines.append(f'{key}={text}\n')
    with path.open('a', encoding='utf-8', newline='') as output:
        output.writelines(lines)


def verify_package(package, archive, selection, harness_source, output):
    package = Path(package)
    archive = Path(archive)
    require(not package.is_symlink() and package.is_dir(), 'package root must be a real directory')
    require(not archive.is_symlink() and archive.is_file(), 'package archive must be a regular file')
    require(isinstance(selection, dict), 'invalid selected package provenance')
    package_source = selection.get('package_source_sha')
    require(isinstance(package_source, str) and COMMIT.fullmatch(package_source),
            'invalid selected package source')
    require(isinstance(harness_source, str) and COMMIT.fullmatch(harness_source),
            'invalid diagnostic harness source')
    info_path = package / 'BUILD-INFO.json'
    require(not info_path.is_symlink() and info_path.is_file(), 'package BUILD-INFO is missing')
    info = json.loads(info_path.read_text(encoding='utf-8'))
    release.verify_build_info(info)
    require(info['source_commit'] == package_source, 'package BUILD-INFO source differs from selected run')
    require(info['target'] == TARGET, 'package BUILD-INFO target is not Windows MSVC')
    package_name = f'vibeface-{info["version"]}-{TARGET}'
    require(package.name == package_name and archive.name == package_name + '.tar.gz',
            'package archive and directory names do not match BUILD-INFO')

    manifest_path = package / 'FILES.sha256'
    require(not manifest_path.is_symlink() and manifest_path.is_file(), 'package file manifest is missing')
    packaged_files = release.files(package, ('FILES.sha256',))
    manifest = ''.join(f'{digest}  {name}\n' for name, digest in packaged_files.items())
    require(manifest_path.read_text(encoding='ascii') == manifest,
            'package files differ from the sealed package manifest')

    checksum_path = archive.with_name(archive.name + '.sha256')
    require(not checksum_path.is_symlink() and checksum_path.is_file(), 'package checksum is missing')
    archive_sha256 = release.digest(archive)
    require(checksum_path.read_text(encoding='ascii') == f'{archive_sha256}  {archive.name}\n',
            'package archive differs from its checksum')
    require(selection.get('artifact_name') == f'node-package-{TARGET}-{package_source}',
            'selected artifact name does not match package source')
    run_id = positive_id(selection.get('run_id'), 'selected run ID')
    artifact_id = positive_id(selection.get('artifact_id'), 'selected package artifact ID')
    require(isinstance(selection.get('artifact_digest'), str)
            and ARTIFACT_DIGEST.fullmatch(selection['artifact_digest']),
            'selected artifact digest is invalid')
    receipt = {
        'schema': 1,
        'diagnostic_only': True,
        'harness_source_commit': harness_source,
        'package_source_commit': package_source,
        'target': TARGET,
        'package_run_id': run_id,
        'package_artifact_id': artifact_id,
        'package_artifact_name': selection['artifact_name'],
        'package_artifact_digest': selection['artifact_digest'],
        'package_archive': archive.name,
        'package_archive_sha256': archive_sha256,
    }
    output = Path(output)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(receipt, indent=2, sort_keys=True) + '\n', encoding='utf-8')
    return receipt


def selection_self_test():
    import tempfile

    now = datetime.datetime(2026, 11, 7, tzinfo=datetime.timezone.utc)
    run_id, artifact_id = 42361414844, 13560000001
    source = 'a' * 40
    repository = release.GITHUB_REPO
    name = f'node-package-{TARGET}-{source}'
    run = {
        'id': run_id, 'path': WORKFLOW_PATH + '@refs/heads/main', 'status': 'completed',
        'conclusion': 'failure', 'head_sha': source,
        'repository': {'full_name': repository, 'id': 1421929651},
        'head_repository': {'full_name': repository},
    }
    jobs = {'total_count': 1, 'jobs': [{
        'name': PACKAGE_JOB, 'status': 'completed', 'conclusion': 'success', 'head_sha': source,
    }]}
    artifacts = {'total_count': 1, 'artifacts': [{
        'id': artifact_id, 'name': name, 'size_in_bytes': 123,
        'expired': False, 'expires_at': '2026-12-01T00:00:00Z',
        'digest': 'sha256:' + 'b' * 64,
        'workflow_run': {'id': run_id, 'head_sha': source, 'repository_id': 1421929651},
    }]}
    selected = validate_selection(str(run_id), repository, run, jobs, artifacts, now=now)
    assert selected['package_source_sha'] == source and selected['artifact_id'] == artifact_id
    verify_harness_source('c' * 40, 'c' * 40)
    try:
        verify_harness_source('c' * 40, 'd' * 40)
    except ValueError as error:
        assert 'checkout differs' in str(error)
    else:
        raise AssertionError('accepted a receipt from another harness checkout')

    def rejects(change, message=None):
        altered_run, altered_jobs, altered_artifacts = copy.deepcopy((run, jobs, artifacts))
        change(altered_run, altered_jobs, altered_artifacts)
        try:
            validate_selection(str(run_id), repository, altered_run, altered_jobs, altered_artifacts, now=now)
        except ValueError as error:
            assert message is None or message in str(error), str(error)
        else:
            raise AssertionError('accepted invalid prior-run package provenance')

    rejects(lambda r, j, a: r.update(repository={'full_name': 'fork/vibeface'}))
    rejects(lambda r, j, a: r.update(head_repository={'full_name': 'fork/vibeface'}))
    rejects(lambda r, j, a: r.update(path='.github/workflows/release.yml'))
    rejects(lambda r, j, a: r.update(status='in_progress'))
    rejects(lambda r, j, a: j['jobs'][0].update(conclusion='failure'))
    rejects(lambda r, j, a: j.update(total_count=2))
    rejects(lambda r, j, a: j['jobs'][0].update(head_sha='c' * 40))
    rejects(lambda r, j, a: a.update(artifacts=[], total_count=0))
    rejects(lambda r, j, a: a['artifacts'][0].update(expired=True))
    rejects(lambda r, j, a: a['artifacts'][0].update(expires_at='2026-11-06T00:00:00Z'))
    rejects(lambda r, j, a: a['artifacts'][0].update(name='node-package-other-' + source))
    rejects(lambda r, j, a: a['artifacts'][0].update(workflow_run={'id': run_id + 1}))
    rejects(lambda r, j, a: a['artifacts'][0]['workflow_run'].update(repository_id=r['repository']['id'] + 1))
    rejects(lambda r, j, a: a.update(artifacts=a['artifacts'] * 2, total_count=2), 'ambiguous')
    rejects(lambda r, j, a: a.update(total_count=2))
    for invalid in ('', '0', '-1', '01', '1x', str(MAX_ID + 1)):
        try:
            run_id_argument(invalid)
        except ValueError:
            pass
        else:
            raise AssertionError('accepted invalid workflow run ID')

    with tempfile.TemporaryDirectory(prefix='windows-diagnostic-self-test-') as temporary:
        root = Path(temporary)
        package = root / f'vibeface-{release.canonical_product_version()}-{TARGET}'
        package.mkdir()
        info = {
            'version': release.canonical_product_version(), 'target': TARGET,
            'source_commit': source, 'update_protocol': release.control_constant('UPDATE_PROTOCOL'),
            'data_epoch': release.control_constant('DATA_EPOCH'),
            'core_version': release.canonical_product_version(),
            'core_source_commit': source, 'app_version': release.canonical_product_version(),
            'app_source_commit': source,
        }
        (package / 'BUILD-INFO.json').write_text(json.dumps(info) + '\n', encoding='utf-8')
        (package / 'vibeface.exe').write_bytes(b'fixture cli')
        (package / 'vibeface-core.exe').write_bytes(b'fixture core')
        hashes = release.files(package, ('FILES.sha256',))
        (package / 'FILES.sha256').write_text(
            ''.join(f'{digest}  {path}\n' for path, digest in hashes.items()), encoding='ascii'
        )
        archive = root / (package.name + '.tar.gz')
        archive.write_bytes(b'fixture archive')
        archive_sha = release.digest(archive)
        archive.with_name(archive.name + '.sha256').write_text(
            f'{archive_sha}  {archive.name}\n', encoding='ascii'
        )
        selected_package = selected | {'package_source_sha': source}
        receipt = verify_package(
            package, archive, selected_package, 'c' * 40, root / 'diagnostic-origin.json'
        )
        assert receipt['diagnostic_only'] is True
        assert receipt['harness_source_commit'] == 'c' * 40 and receipt['package_source_commit'] == source
        bad_info = dict(info, source_commit='d' * 40, core_source_commit='d' * 40, app_source_commit='d' * 40)
        (package / 'BUILD-INFO.json').write_text(json.dumps(bad_info) + '\n', encoding='utf-8')
        changed_files = release.files(package, ('FILES.sha256',))
        (package / 'FILES.sha256').write_text(
            ''.join(f'{digest}  {path}\n' for path, digest in changed_files.items()), encoding='ascii'
        )
        try:
            verify_package(package, archive, selected_package, 'c' * 40, root / 'foreign.json')
        except ValueError as error:
            assert 'source differs' in str(error)
        else:
            raise AssertionError('accepted a package from a source other than its selected run')
        (package / 'BUILD-INFO.json').write_text(json.dumps(info) + '\n', encoding='utf-8')
        (package / 'vibeface.exe').write_bytes(b'changed')
        try:
            verify_package(package, archive, selected_package, 'c' * 40, root / 'bad.json')
        except ValueError as error:
            assert 'sealed package manifest' in str(error)
        else:
            raise AssertionError('accepted package bytes changed after sealing')


def select_command(args):
    run = json.loads(args.run_json.read_text(encoding='utf-8'))
    jobs = json.loads(args.jobs_json.read_text(encoding='utf-8'))
    artifacts = json.loads(args.artifacts_json.read_text(encoding='utf-8'))
    selected = validate_selection(args.run_id, args.repository, run, jobs, artifacts)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(selected, indent=2, sort_keys=True) + '\n', encoding='utf-8')
    write_output(args.github_output, {
        'package_source_sha': selected['package_source_sha'],
        'package_artifact_id': selected['artifact_id'],
        'package_artifact_name': selected['artifact_name'],
        'package_artifact_digest': selected['artifact_digest'],
    })


def verify_command(args):
    selection = json.loads(args.selection.read_text(encoding='utf-8'))
    checkout = subprocess.check_output(
        ['git', '-C', str(Path(__file__).resolve().parent.parent), 'rev-parse', 'HEAD'], text=True
    ).strip()
    verify_harness_source(args.harness_source, checkout)
    verify_package(args.package, args.archive, selection, args.harness_source, args.output)


def main():
    parser = argparse.ArgumentParser()
    subparsers = parser.add_subparsers(dest='command', required=True)
    select = subparsers.add_parser('select')
    select.add_argument('--run-id', required=True)
    select.add_argument('--repository', required=True)
    select.add_argument('--run-json', type=Path, required=True)
    select.add_argument('--jobs-json', type=Path, required=True)
    select.add_argument('--artifacts-json', type=Path, required=True)
    select.add_argument('--output', type=Path, required=True)
    select.add_argument('--github-output', type=Path)
    select.set_defaults(func=select_command)
    verify = subparsers.add_parser('verify-package')
    verify.add_argument('--package', type=Path, required=True)
    verify.add_argument('--archive', type=Path, required=True)
    verify.add_argument('--selection', type=Path, required=True)
    verify.add_argument('--harness-source', required=True)
    verify.add_argument('--output', type=Path, required=True)
    verify.set_defaults(func=verify_command)
    if '--self-test' in sys.argv:
        selection_self_test()
        print('Windows prior-run package selector self-tests passed')
        return
    args = parser.parse_args()
    args.func(args)


if __name__ == '__main__':
    main()
