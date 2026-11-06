#!/usr/bin/env node
/**
 * Stands in for `vibeface-core --control` with the protocol the real binary implements:
 * one bounded JSON request on stdin, one `{ok,result}` or `{ok,error}` line on stdout.
 * Field names and shapes copy `apps/core/src/control.rs`; nothing here is invented.
 */
import { createHash } from 'node:crypto'
import { existsSync, mkdirSync, writeFileSync } from 'node:fs'
import { join } from 'node:path'
import { stdin, stdout } from 'node:process'

const LISTEN = process.env['FIXTURE_LISTEN'] ?? '127.0.0.1:4790'
const LOCAL_LISTEN = process.env['FIXTURE_LOCAL_LISTEN'] ?? '127.0.0.1:4791'
const forcedExitCodeRaw = process.env['FIXTURE_CONTROL_EXIT_CODE']
const forcedExitCode =
  forcedExitCodeRaw === undefined || forcedExitCodeRaw === '' ? null : Number.parseInt(forcedExitCodeRaw, 10)
const forcedSignal = process.env['FIXTURE_CONTROL_EXIT_SIGNAL'] ?? null

const deviceId = (dataDir) => createHash('sha256').update(dataDir).digest('hex')

function finish() {
  if (forcedSignal) {
    try {
      process.kill(process.pid, forcedSignal)
      return
    } catch {
      process.exit(1)
    }
  }
  if (forcedExitCode !== null && Number.isFinite(forcedExitCode)) {
    process.exit(forcedExitCode)
    return
  }
  process.exit(0)
}

function reply(value) {
  stdout.write(`${JSON.stringify(value)}\n`, () => finish())
}

const chunks = []
for await (const chunk of stdin) chunks.push(chunk)
let request
try {
  request = JSON.parse(Buffer.concat(chunks).toString('utf8'))
} catch {
  reply({ ok: false, error: 'invalid core control request' })
}

const dataDir = typeof request.data_dir === 'string' ? request.data_dir : request.options?.data_dir
if (typeof dataDir !== 'string') reply({ ok: false, error: 'data directory is required' })

const initialized = () => existsSync(join(dataDir, 'config.json')) && existsSync(join(dataDir, 'identity.json'))
const running = () => existsSync(join(dataDir, 'node.running'))

if (request.command === 'init') {
  if (initialized()) reply({ ok: false, error: 'node already initialized; existing identity and configuration were preserved' })
  mkdirSync(dataDir, { recursive: true })
  writeFileSync(join(dataDir, 'config.json'), '{}')
  writeFileSync(join(dataDir, 'identity.json'), '{}')
  writeFileSync(join(dataDir, 'data-epoch.json'), JSON.stringify({ data_epoch: 1 }))
  reply({ ok: true, result: { data_dir: dataDir, web_enabled: true, device_id: deviceId(dataDir) } })
}

if (request.command === 'inspect') {
  if (!initialized()) reply({ ok: false, error: 'initialized data directory is required; run vibeface init first' })
  reply({
    ok: true,
    result: {
      status: running() ? 'running' : 'stopped',
      device_id: deviceId(dataDir),
      data_dir: dataDir,
      maintenance_socket: join('/tmp', `vibeface-${deviceId(dataDir).slice(0, 32)}`, 'maintenance.sock'),
      listen: LISTEN,
      local_listen: LOCAL_LISTEN,
      public_url: `http://${LISTEN}`,
      public_entry: false,
      web_enabled: true,
      version: running() ? '0.0.1-fixture' : null,
      write_state: null,
      peers: null,
      core_version: '0.0.1-fixture',
      error: running() ? null : 'Connection refused (os error 61)',
    },
  })
}

if (request.command === 'access') {
  if (!running()) reply({ ok: false, error: 'local node is unavailable; start it before opening browser access' })
  const node = deviceId(dataDir)
  const token = createHash('sha256').update(`${dataDir}:token`).digest('hex')
  reply({
    ok: true,
    result: {
      url: `http://localhost:${LISTEN.split(':').pop()}/#local=${token}&node=${node}`,
      device_id: node,
      local: true,
    },
  })
}

reply({ ok: false, error: 'unsupported command' })
