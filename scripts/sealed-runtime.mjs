import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import * as fs from 'node:fs';
import path from 'node:path';

export const hash = file => createHash('sha256').update(fs.readFileSync(file)).digest('hex');

export function relativeFiles(directory) {
  return fs.readdirSync(directory, { recursive: true, withFileTypes: true })
    .filter(entry => entry.isFile() || entry.isSymbolicLink())
    .map(entry => path.relative(directory, path.join(entry.parentPath, entry.name)).split(path.sep).join('/'))
    .sort();
}

export function verifySealedRuntime(directory, sealed) {
  assert.deepEqual(relativeFiles(directory).filter(name => name !== 'FILES.sha256'), [...sealed.keys()].sort(),
    'packaging changed the set of runtime files');
  for (const [name, expected] of sealed) {
    const file = path.join(directory, name);
    assert(!fs.lstatSync(file).isSymbolicLink(), 'the runtime must not contain links');
    assert.equal(hash(file), expected, `packaging or signing changed the sealed runtime file ${name}`);
  }
}

export function readSealedRuntime(directory) {
  const sealed = new Map();
  for (const line of fs.readFileSync(path.join(directory, 'FILES.sha256'), 'utf8').trimEnd().split('\n')) {
    const match = /^([a-f0-9]{64})  ([a-zA-Z0-9._/-]+)$/.exec(line);
    assert(match && match[2] !== 'FILES.sha256' && !sealed.has(match[2]), 'invalid or duplicate runtime checksum entry');
    assert(match[2].split('/').every(part => part && part !== '.' && part !== '..'), 'unsafe runtime checksum path');
    sealed.set(match[2], match[1]);
  }
  verifySealedRuntime(directory, sealed);
  return sealed;
}
