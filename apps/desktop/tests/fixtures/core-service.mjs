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

function stop(code) {
  rmSync(marker, { force: true })
  process.exit(code)
}

if (crashAfter !== null && Number.isFinite(crashAfter)) {
  setTimeout(() => stop(3), crashAfter)
} else {
  writeFileSync(marker, String(process.pid))
}

stdin.resume()
stdin.on('end', () => stop(0))
stdin.on('close', () => stop(0))
setInterval(() => {}, 1 << 30)
