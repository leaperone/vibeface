// Pure release facts and grammar. No filesystem, environment, process or network access.
import assert from 'node:assert/strict';

export const PLATFORMS = Object.freeze(Object.fromEntries([
  ['aarch64-apple-darwin', { host: 'darwin-arm64', platform: 'darwin', flag: '--mac', installer: 'dmg', exe: '', service: 'sh', signing: 'developer-id-notarized' }],
  ['aarch64-unknown-linux-gnu', { host: 'linux-arm64', platform: 'linux', flag: '--linux', installer: 'AppImage', exe: '', service: 'sh', signing: 'platform-checksums', arch: 'arm64' }],
  ['x86_64-unknown-linux-gnu', { host: 'linux-x64', platform: 'linux', flag: '--linux', installer: 'AppImage', exe: '', service: 'sh', signing: 'platform-checksums', arch: 'x64' }],
  ['x86_64-pc-windows-msvc', { host: 'win32-x64', platform: 'win32', flag: '--win', installer: 'exe', exe: '.exe', service: 'ps1', signing: 'authenticode' }],
].map(([target, facts]) => [target, Object.freeze(facts)])));
export const DESKTOP_SCHEMA = 2;
export const SIGNING_STATES = Object.freeze(['developer-id-notarized', 'authenticode', 'unsigned-development-preview', 'platform-checksums']);
export const NATIVE_UI = Object.freeze({ ui: 'gpui', gpui_kit: '0.9.1', gpui: '0.3.12' });
const BUILD_FIELDS = Object.freeze(['version', 'target', 'source_commit', 'update_protocol', 'data_epoch',
  'core_version', 'core_source_commit', 'app_version', 'app_source_commit']);
const APPS = Object.freeze(['core', 'app', 'cli', 'desktop', 'mobile', 'www']);

/**
 * Load the release-facing portion of the parsed AUTO-16 runtime contract.
 *
 * This deliberately accepts data supplied by the caller.  The release-format
 * library stays detached from repository paths and fixture files; source
 * adapters may parse the versioned contract and pass it here.  The returned
 * value is a defensive, deeply frozen projection containing only the fields,
 * targets and role paths used by release consumers.
 */
export function loadRuntimeDefinitions(contract) {
  assert(contract && typeof contract === 'object' && !Array.isArray(contract), 'invalid runtime contract');
  assert(contract.schema === 1, 'unsupported runtime contract schema');
  const fields = contract.build_info?.fields;
  assert(Array.isArray(fields) && fields.length > 0 && fields.every(field => typeof field === 'string' && field),
    'invalid runtime BUILD-INFO fields');
  assert(new Set(fields).size === fields.length, 'duplicate runtime BUILD-INFO field');
  const web = contract.files?.web_required;
  assert(Array.isArray(web) && web.every(name => typeof name === 'string' && name), 'invalid runtime Web files');
  assert(new Set(web).size === web.length, 'duplicate runtime Web file');
  assert(Array.isArray(contract.targets) && contract.targets.length > 0, 'invalid runtime targets');
  const targets = contract.targets.map(row => {
    assert(row && typeof row === 'object' && !Array.isArray(row), 'invalid runtime target');
    const expected = ['target', 'host', 'platform', 'flag', 'exe_suffix', 'service_suffix', 'installer', 'signing', 'arch', 'programs', 'roles'];
    assert.deepEqual(Object.keys(row).sort(), [...expected].sort(), 'unsupported runtime target fields');
    for (const field of ['target', 'host', 'platform', 'flag', 'service_suffix', 'installer', 'signing']) {
      assert(typeof row[field] === 'string' && row[field], `invalid runtime target ${field}`);
    }
    assert(typeof row.exe_suffix === 'string', 'invalid runtime target exe_suffix');
    assert(row.arch === null || typeof row.arch === 'string', 'invalid runtime target arch');
    assert(Array.isArray(row.programs) && row.programs.every(name => typeof name === 'string' && name), 'invalid runtime programs');
    assert(new Set(row.programs).size === row.programs.length, 'duplicate runtime program');
    assert(row.roles && typeof row.roles === 'object' && !Array.isArray(row.roles), 'invalid runtime roles');
    assert.deepEqual(Object.keys(row.roles).sort(), ['desktop-minimum', 'node-full', 'npm-vendor', 'runtime-core'], 'unsupported runtime roles');
    const roles = Object.fromEntries(Object.entries(row.roles).map(([name, paths]) => {
      assert(name && Array.isArray(paths) && paths.every(path => typeof path === 'string' && path), 'invalid runtime role paths');
      assert(new Set(paths).size === paths.length, `duplicate runtime role path: ${name}`);
      return [name, Object.freeze([...paths])];
    }));
    return Object.freeze({ ...row, programs: Object.freeze([...row.programs]), roles: Object.freeze(roles) });
  });
  assert(new Set(targets.map(row => row.target)).size === targets.length, 'duplicate runtime target');
  return Object.freeze({ schema: contract.schema, fields: Object.freeze([...fields]),
    web_required: Object.freeze([...web]), targets: Object.freeze(targets) });
}

export function parseReleaseVersion(value) {
  assert(typeof value === 'string', 'invalid component release version');
  const match = /^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-(hourly)\.([0-9]{10})|-(rc)\.([1-9][0-9]*))?$/.exec(value);
  assert(match && match[0] === value, 'invalid component release version');
  const channel = match[4] ? 'hourly' : match[6] ? 'rc' : 'stable';
  if (channel === 'hourly') {
    const stamp = match[5];
    const [year, month, day, hour] = [stamp.slice(0, 4), stamp.slice(4, 6), stamp.slice(6, 8), stamp.slice(8)].map(Number);
    const leap = year % 4 === 0 && (year % 100 !== 0 || year % 400 === 0);
    const days = [31, leap ? 29 : 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    assert(year >= 1 && year <= 9999 && month >= 1 && month <= 12 && day >= 1 && day <= days[month - 1] && hour <= 23,
      'invalid hourly date');
  }
  return Object.freeze({ value, channel, parts: Object.freeze([match[1], match[2], match[3],
    ...(match[5] || match[7] ? [match[5] ?? match[7]] : [])].map(BigInt)) });
}

export function parseReleaseTag(tag) {
  assert(typeof tag === 'string', 'invalid release tag');
  const match = /^(core|app|cli|desktop|mobile|www)-v(.+)$/.exec(tag);
  assert(match && match[0] === tag, 'invalid release tag');
  const parsed = parseReleaseVersion(match[2]);
  return Object.freeze({ app: match[1], version: parsed.value, channel: parsed.channel });
}

export function npmDistTag(channel) {
  assert(['stable', 'rc', 'hourly'].includes(channel), 'invalid release channel');
  return channel === 'stable' ? 'latest' : channel;
}

export function compareWithinChannel(left, right) {
  const after = parseReleaseVersion(left), before = parseReleaseVersion(right);
  assert.equal(after.channel, before.channel, 'npm dist-tag must point to its own CLI channel');
  return after.parts.map((part, index) => part - before.parts[index]).find(part => part !== 0n) ?? 0n;
}

export function platformFor(target) {
  assert(Object.hasOwn(PLATFORMS, target), `unsupported release target: ${target}`);
  return PLATFORMS[target];
}

export function programNames(target) {
  const { exe } = platformFor(target);
  return [`vibeface${exe}`, `vibeface-core${exe}`];
}

/**
 * Files that every native Node runtime archive must carry before it is sealed.
 *
 * The Web payload is supplied by the caller because the Web manifest is owned
 * by the Node staging path. Keeping the platform and support-file facts here
 * prevents npm verification from drifting from the native packagers while
 * leaving desktop's smaller runtime minimum independent.
 */
export function requiredRuntimeFiles(target, { web = [], checksum = false } = {}) {
  const files = [
    ...programNames(target),
    'BUILD-INFO.json',
    'README.md',
    'RUNTIME-REQUIREMENTS.md',
    `scripts/install-node-service.${platformFor(target).service}`,
    'docs/node.md',
    'docs/releasing.md',
    ...web.map(name => `web/${name}`),
  ];
  return checksum ? ['FILES.sha256', ...files] : files;
}

export function desktopPayloadNames(version, target) {
  parseReleaseVersion(version);
  const { installer } = platformFor(target);
  const base = `vibeface-desktop-${version}-${target}`;
  return [`${base}.${installer}`, `${base}.zip`];
}

export function validateBuildInfoShape(info, contracts) {
  assert(info && typeof info === 'object' && !Array.isArray(info), 'invalid build info');
  assert.deepEqual(Object.keys(info).sort(), [...BUILD_FIELDS].sort(), 'unsupported build info fields');
  for (const field of ['version', 'core_version', 'app_version']) parseReleaseVersion(info[field]);
  platformFor(info.target);
  for (const field of ['source_commit', 'core_source_commit', 'app_source_commit']) {
    assert(typeof info[field] === 'string' && info[field].length === 40 && /^[a-f0-9]{40}$/.test(info[field]), 'invalid component source commit');
  }
  assert(contracts && Object.keys(contracts).length === 2, 'explicit release contracts required');
  for (const field of ['data_epoch', 'update_protocol']) {
    assert(Number.isSafeInteger(contracts[field]) && contracts[field] >= 0 && contracts[field] <= 0xffffffff, 'invalid release contract');
    assert.equal(info[field], contracts[field], 'unsupported runtime protocol or data epoch');
  }
  return info;
}

export function releaseApp(value) {
  assert(APPS.includes(value), 'unsupported release application');
  return value;
}
