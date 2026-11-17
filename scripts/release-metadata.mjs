// Scheme A: the sole Node source adapter. This is deliberately a narrow reader, not a
// general Rust/TOML parser or compiled Rust authority export. Unsupported declarations fail.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { NATIVE_UI, parseReleaseVersion, releaseApp } from './release-contract.mjs';

// JSON.parse alone accepts duplicate keys. Walk the original tokens first, including nested
// objects and escaped keys, so source manifests cannot silently select the last declaration.
export function parseSourceJson(source) {
  let offset = 0;
  const space = () => { while (/\s/.test(source[offset] ?? '') && offset < source.length) offset++; };
  const string = () => {
    const start = offset++;
    while (offset < source.length) {
      const char = source[offset++];
      if (char === '\\') offset++;
      else if (char === '"') return JSON.parse(source.slice(start, offset));
    }
    assert.fail('unterminated JSON string');
  };
  const value = () => {
    space();
    if (source[offset] === '"') { string(); return; }
    if (source[offset] === '{') {
      offset++; space(); const keys = new Set();
      if (source[offset] === '}') { offset++; return; }
      while (true) {
        space(); assert.equal(source[offset], '"', 'invalid JSON key');
        const key = string(); assert(!keys.has(key), `duplicate JSON key: ${key}`); keys.add(key);
        space(); assert.equal(source[offset++], ':', 'invalid JSON object'); value(); space();
        if (source[offset] === '}') { offset++; return; }
        assert.equal(source[offset++], ',', 'invalid JSON object');
      }
    }
    if (source[offset] === '[') {
      offset++; space(); if (source[offset] === ']') { offset++; return; }
      while (true) { value(); space(); if (source[offset] === ']') { offset++; return; } assert.equal(source[offset++], ',', 'invalid JSON array'); }
    }
    const token = /^(?:true|false|null|-?(?:0|[1-9][0-9]*)(?:\.[0-9]+)?(?:[eE][+-]?[0-9]+)?)/.exec(source.slice(offset));
    assert(token, 'invalid JSON value'); offset += token[0].length;
  };
  value(); space(); assert.equal(offset, source.length, 'invalid trailing JSON');
  return JSON.parse(source);
}

function table(source, name) {
  const sections = [...source.matchAll(/^\s*(\[\[?[^\n]+\]\]?)\s*(?:#.*)?$/gm)];
  assert(!sections.some(section => section[1] !== `[${name}]` && (section[1].replace(/[\"'\s]/g, '') === `[${name}]` || section[1].replace(/[\"'\s]/g, '').startsWith(`[${name}.`))), `unsupported TOML [${name}] declaration`);
  const matches = sections.filter(section => section[1] === `[${name}]`);
  assert.equal(matches.length, 1, `exactly one TOML [${name}] table required`);
  if (name === 'package') assert(source.slice(0, sections[0].index).split('\n').every(line => !line.trim() || line.trimStart().startsWith('#')), 'unsupported TOML package preamble');
  const index = sections.indexOf(matches[0]);
  const body = source.slice(matches[0].index + matches[0][0].length, sections[index + 1]?.index ?? source.length);
  const entries = new Map();
  for (const line of body.split('\n')) {
    if (!line.trim() || line.trimStart().startsWith('#')) continue;
    const entry = /^\s*([a-zA-Z0-9_-]+(?:\.[a-zA-Z0-9_-]+)*)\s*=\s*(.*?)\s*$/.exec(line);
    assert(entry, `unsupported TOML declaration in [${name}]`);
    assert(!entries.has(entry[1]), `duplicate TOML key: ${entry[1]}`);
    entries.set(entry[1], entry[2]);
  }
  return entries;
}

function literal(entries, key) {
  assert(![...entries.keys()].some(name => name.startsWith(`${key}.`)), `unsupported TOML dotted declaration: ${key}`);
  const match = /^"([^"\\\r\n]*)"(?:\s+#.*)?$/.exec(entries.get(key) ?? '');
  assert(match, `explicit TOML string required: ${key}`);
  return match[1];
}

// Preserve positions while removing Rust comments and string contents from declaration scans.
function rustCode(source) {
  let offset = 0, result = '';
  const hide = end => { result += source.slice(offset, end).replace(/[^\n]/g, ' '); offset = end; };
  while (offset < source.length) {
    if (source.startsWith('//', offset)) { const end = source.indexOf('\n', offset); hide(end < 0 ? source.length : end); }
    else if (source.startsWith('/*', offset)) {
      let end = offset + 2, depth = 1;
      while (end < source.length && depth) {
        if (source.startsWith('/*', end)) { depth++; end += 2; }
        else if (source.startsWith('*/', end)) { depth--; end += 2; }
        else end++;
      }
      assert.equal(depth, 0, 'unterminated Rust comment'); hide(end);
    } else {
      const raw = /^(?:br|r)(#*)"/.exec(source.slice(offset));
      if (raw) { const end = source.indexOf('"' + raw[1], offset + raw[0].length); assert(end >= 0, 'unterminated Rust raw string'); hide(end + 1 + raw[1].length); }
      else if (source[offset] === '"') {
        let end = offset + 1;
        while (end < source.length && source[end] !== '"') { if (source[end] === '\\') end++; end++; }
        assert(end < source.length, 'unterminated Rust string'); hide(end + 1);
      } else result += source[offset++];
    }
  }
  return result;
}

export function readContracts(root) {
  const source = readFileSync(path.join(root, 'crates/control/src/lib.rs'), 'utf8');
  const code = rustCode(source);
  // Inner attributes can condition the whole crate or enclosing module, regardless of
  // intervening items. This narrow reader supports no inner attributes at all.
  assert(!/#\s*!\s*\[/.test(code), 'inner Rust attributes are unsupported');
  const result = {};
  for (const [field, name] of [['data_epoch', 'DATA_EPOCH'], ['update_protocol', 'UPDATE_PROTOCOL']]) {
    // Count every declaration spelling, then require the one supported unconditional literal.
    const declarations = [...code.matchAll(new RegExp(`\\bconst\\s+${name}\\b`, 'g'))];
    assert.equal(declarations.length, 1, `exactly one Rust ${name} declaration required`);
    const prefix = code.slice(0, declarations[0].index).split('\n');
    assert.equal(prefix.at(-1), 'pub ', `unsupported Rust ${name} declaration`);
    const before = code.slice(0, declarations[0].index);
    const boundary = Math.max(before.lastIndexOf(';'), before.lastIndexOf('}'), before.lastIndexOf('{'));
    assert(!/#\s*\[/.test(before.slice(boundary + 1)), `attributed Rust ${name} is unsupported`);
    assert.equal([...before].reduce((depth, char) => depth + (char === '{' ? 1 : char === '}' ? -1 : 0), 0), 0, `nested Rust ${name} is unsupported`);
    const match = new RegExp(`^pub const ${name}: u32 = (0|[1-9][0-9]*);$`, 'm').exec(source);
    assert(match && match.index + 4 === declarations[0].index, `unsupported Rust ${name} declaration`);
    const number = Number(match[1]);
    assert(Number.isSafeInteger(number) && number <= 0xffffffff, `invalid Rust ${name}`);
    result[field] = number;
  }
  return Object.freeze(result);
}

export function readOwnVersion(root, application) {
  const app = releaseApp(application);
  let version;
  if (['core', 'cli', 'desktop'].includes(app)) {
    const source = readFileSync(path.join(root, `apps/${app}/Cargo.toml`), 'utf8');
    const entries = table(source, 'package');
    assert.equal(literal(entries, 'name'), app === 'cli' ? 'vibeface' : `vibeface-${app}`, 'unexpected Cargo package');
    version = literal(entries, 'version');
  } else {
    const file = app === 'mobile' ? 'app.json' : 'package.json';
    const manifest = parseSourceJson(readFileSync(path.join(root, `apps/${app}/${file}`), 'utf8'));
    version = app === 'mobile' ? manifest.expo?.extra?.releaseVersion : manifest.version;
  }
  return parseReleaseVersion(version).value;
}

export function readNpmTemplate(root) {
  const manifest = parseSourceJson(readFileSync(path.join(root, 'apps/cli/npm/package.json'), 'utf8'));
  assert.deepEqual(Object.keys(manifest).sort(), ['name', 'version', 'private', 'description', 'type', 'bin', 'files', 'engines', 'os', 'cpu', 'repository', 'publishConfig'].sort(), 'unsupported npm source template fields');
  assert.equal(manifest.name, '@leaperone/vibeface');
  assert.equal(manifest.version, '0.0.0', 'npm template is not the CLI own version');
  assert.equal(manifest.private, true);
  assert.deepEqual(manifest.publishConfig, { registry: 'https://registry.npmjs.org/', access: 'public', tag: 'hourly' });
  assert.deepEqual(manifest.bin, { vibeface: 'bin/vibeface.mjs' });
  return manifest;
}

export function readNativeUi(root) {
  const manifest = readFileSync(path.join(root, 'apps/desktop/Cargo.toml'), 'utf8');
  assert.equal(literal(table(manifest, 'dependencies'), 'gpui-kit'), `=${NATIVE_UI.gpui_kit}`, 'desktop must pin the verified GPUI kit');
  const source = readFileSync(path.join(root, 'Cargo.lock'), 'utf8');
  assert([...source.matchAll(/^\s*(\[[^\n]+)/gm)].every(match => match[1].trim() === '[[package]]'), 'unsupported Cargo.lock table');
  const chunks = source.split(/^\[\[package\]\]\s*$/m);
  const preamble = chunks.shift().split('\n').filter(line => line.trim() && !line.trimStart().startsWith('#'));
  assert.deepEqual(preamble, ['version = 4'], 'unsupported or duplicate Cargo.lock version');
  const records = chunks.map(record => {
    const entries = new Map(); let dependencies = null;
    for (const line of record.split('\n')) {
      if (!line.trim() || line.trimStart().startsWith('#')) continue;
      if (dependencies !== null) {
        if (line === ']') { dependencies = null; continue; }
        const dependency = /^\s*"([^"\\]+)",?\s*$/.exec(line);
        assert(dependency && !dependencies.has(dependency[1]), 'unsupported or duplicate locked dependency');
        dependencies.add(dependency[1]); continue;
      }
      const entry = /^([a-z_]+) = (.*)$/.exec(line);
      assert(entry && ['name', 'version', 'source', 'checksum', 'dependencies'].includes(entry[1]), 'unsupported Cargo.lock declaration');
      assert(!entries.has(entry[1]), `duplicate locked ${entry[1]}`);
      if (entry[1] === 'dependencies') {
        assert(['[', '[]'].includes(entry[2]), 'unsupported locked dependencies');
        entries.set(entry[1], new Set());
        if (entry[2] === '[') dependencies = entries.get(entry[1]);
      } else {
        const value = /^"([^"\\]+)"$/.exec(entry[2]);
        assert(value, `explicit locked string required: ${entry[1]}`); entries.set(entry[1], value[1]);
      }
    }
    assert(dependencies === null && entries.has('name') && entries.has('version'), 'incomplete Cargo.lock package');
    return entries;
  });
  for (const [name, version] of [['gpui-kit', NATIVE_UI.gpui_kit], ['gpui-pre', NATIVE_UI.gpui]]) {
    const candidates = records.filter(record => record.get('name') === name);
    assert.equal(candidates.length, 1, `exactly one Cargo.lock package ${name} required`);
    assert.equal(candidates[0].get('version'), version, `unexpected locked ${name} version`);
  }
  return NATIVE_UI;
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    const args = process.argv.slice(2);
    assert(args.length === 2 && args[0] === '--app', 'expected --app <application>');
    const root = fileURLToPath(new URL('../', import.meta.url));
    const app = releaseApp(args[1]);
    console.log(JSON.stringify({ app, version: readOwnVersion(root, app), contracts: readContracts(root),
      ...(app === 'desktop' ? { native_ui: readNativeUi(root) } : {}) }));
  } catch (error) { console.error(`vibeface release metadata: ${error.message}`); process.exitCode = 1; }
}
