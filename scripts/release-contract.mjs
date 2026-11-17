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
