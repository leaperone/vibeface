#!/usr/bin/env node
/**
 * Stands in for a long-running `vibeface-core --data-dir <dir> --desktop-parent-stdin`.
 * It refuses to run without the parent-stdin flag, marks the directory as running while it
 * lives, and shuts down normally when the desktop closes its stdin — the EOF contract the
 * real Core will implement. `FIXTURE_CRASH_AFTER_MS` makes it fail instead.
 */
import { existsSync, rmSync, writeFileSync } from 'node:fs'
import { join } from 'node:path'
import { argv, stdin } from 'node:process'

const args = argv.slice(2)
if (!args.includes('--desktop-parent-stdin')) {
  process.stderr.write('owned core requires --desktop-parent-stdin\n')
  process.exit(2)
}
const dataDir = args[args.indexOf('--data-dir') + 1]
if (!dataDir || !existsSync(dataDir)) {
  process.stderr.write('--data-dir is required\n')
  process.exit(2)
}

const marker = join(dataDir, 'node.running')
const requested = process.env['FIXTURE_CRASH_AFTER_MS']
const crashAfter = requested === undefined || requested === '' ? null : Number(requested)
const ignoreParentEof = process.env['FIXTURE_IGNORE_PARENT_EOF'] === '1'
const ignoreSigterm = process.env['FIXTURE_IGNORE_SIGTERM'] === '1' && process.platform !== 'win32'
const termDelayRaw = process.env['FIXTURE_TERM_DELAY_MS']
const termDelayMs = termDelayRaw === undefined || termDelayRaw === '' ? 0 : Number(termDelayRaw)
let exiting = false

function stop(code) {
  if (exiting) return
  exiting = true
  rmSync(marker, { force: true })
  process.exit(code)
}

if (crashAfter !== null && Number.isFinite(crashAfter)) {
  setTimeout(() => stop(3), crashAfter)
} else {
  writeFileSync(marker, String(process.pid))
}

stdin.resume()
if (!ignoreParentEof) {
  stdin.on('end', () => stop(0))
  stdin.on('close', () => stop(0))
}
if (ignoreSigterm) {
  process.on('SIGTERM', () => {})
} else {
  process.on('SIGTERM', () => {
    if (Number.isFinite(termDelayMs) && termDelayMs > 0) {
      setTimeout(() => stop(0), termDelayMs)
      return
    }
    stop(0)
  })
}
process.on('SIGINT', () => stop(0))
setInterval(() => {}, 1 << 30)
