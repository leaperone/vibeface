import { stdin, stdout } from 'node:process'

const chunks = []
for await (const c of stdin) chunks.push(c)
const line = Buffer.concat(chunks).toString('utf8').trim().split('\n')[0] ?? ''
let req
try {
  req = JSON.parse(line)
} catch {
  stdout.write(JSON.stringify({ ok: false, error: 'invalid request' }) + '\n')
  process.exit(1)
}

if (req.command === 'inspect') {
  const env = process.env.VIBEFACE_DESKTOP_FAKE_INSPECT_JSON
  const payload = env
    ? JSON.parse(env)
    : { status: 'stopped', device_id: 'deadbeef', data_dir: req.data_dir }
  stdout.write(JSON.stringify({ ok: true, result: payload }) + '\n')
  process.exit(0)
}
if (req.command === 'access') {
  stdout.write(
    JSON.stringify({
      ok: true,
      result: { url: 'http://127.0.0.1:4790/', device_id: 'deadbeef', local: false },
    }) + '\n'
  )
  process.exit(0)
}
stdout.write(JSON.stringify({ ok: false, error: 'unsupported command' }) + '\n')
process.exit(1)
