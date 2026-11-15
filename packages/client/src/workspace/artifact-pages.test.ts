import assert from 'node:assert/strict'
import test from 'node:test'
import { parseArtifactPage, parseBinaryPage } from './model.ts'

test('binary pages must advance within the declared artifact size', () => {
  for (const page of [
    { blob_hash: 'hash', offset: 0, next_offset: 0, size: 10, data: '' },
    { blob_hash: 'hash', offset: 0, next_offset: 3, size: 1, data: 'YWJj' },
    { blob_hash: 'hash', offset: 2, next_offset: 1, size: 10, data: '' },
    { blob_hash: 'hash', offset: 5, next_offset: 6, size: 1, data: 'YQ==' },
    { blob_hash: 'hash', offset: -1, next_offset: null, size: 0, data: 'YQ==' },
  ]) {
    assert.throws(() => parseBinaryPage(page, 'hash', page.offset), JSON.stringify(page))
  }
})

test('text and binary pages reject responses for another hash or offset and incomplete payloads', () => {
  for (const change of [
    { blob_hash: 'other' }, { offset: 1 }, { size: -1 }, { size: 1.5 },
    { next_offset: undefined }, { next_offset: -1 }, { next_offset: 3 }, { next_offset: null },
  ]) {
    const range = { blob_hash: 'hash', offset: 0, next_offset: 1, size: 2, ...change }
    assert.throws(() => parseBinaryPage({ ...range, data: 'YQ==' }, 'hash', 0))
    assert.throws(() => parseArtifactPage({ ...range, text: 'a' }, 'hash', 0))
  }
})

test('paged and terminal binary responses preserve bytes and the terminal null cursor', () => {
  assert.deepEqual(parseBinaryPage({ blob_hash: 'hash', offset: 0, next_offset: 2, size: 3, data: 'AAE=' }, 'hash', 0),
    { bytes: new Uint8Array([0, 1]), next_offset: 2, size: 3 })
  assert.deepEqual(parseBinaryPage({ blob_hash: 'hash', offset: 2, next_offset: null, size: 3, data: '/w==' }, 'hash', 2),
    { bytes: new Uint8Array([255]), next_offset: null, size: 3 })
  assert.deepEqual(parseBinaryPage({ blob_hash: 'empty', offset: 0, next_offset: null, size: 0, data: '' }, 'empty', 0),
    { bytes: new Uint8Array(), next_offset: null, size: 0 })
})

test('text pagination measures UTF-8 bytes and permits an empty terminal page', () => {
  assert.deepEqual(parseArtifactPage({ blob_hash: 'hash', offset: 0, next_offset: 3, size: 4, text: '中' }, 'hash', 0),
    { text: '中', offset: 0, next_offset: 3, size: 4 })
  assert.deepEqual(parseArtifactPage({ blob_hash: 'hash', offset: 3, next_offset: null, size: 4, text: 'a' }, 'hash', 3),
    { text: 'a', offset: 3, next_offset: null, size: 4 })
  assert.deepEqual(parseArtifactPage({ blob_hash: 'empty', offset: 0, next_offset: null, size: 0, text: '' }, 'empty', 0),
    { text: '', offset: 0, next_offset: null, size: 0 })
  assert.throws(() => parseArtifactPage({ blob_hash: 'hash', offset: 0, next_offset: 1, size: 4, text: '中' }, 'hash', 0))
})
