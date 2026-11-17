import { Directory, File, Paths } from 'expo-file-system'
import { createMMKV } from 'react-native-mmkv'
import { record } from '@vibeface/client/lib/node.ts'

const recovery = new Directory(Paths.document, 'recovery')
const selections = new Directory(Paths.cache, 'vibeface-selections')
const transfers = new Directory(Paths.document, 'transfers')
const drafts = new Directory(Paths.document, 'chat-drafts')
const attachments = new Directory(Paths.document, 'chat-attachments')
const index = createMMKV({ id: 'vibeface-private-files' })
const cachePrefix = `${Paths.cache.uri.replace(/\/$/, '')}/`
const selectionPrefix = `${selections.uri.replace(/\/$/, '')}/`
const recoveryPrefix = `${recovery.uri.replace(/\/$/, '')}/`
const pending = new Set<Promise<unknown>>()
let erasing = false
let epoch = 0

const changed = () => new Error('应用内数据正在清除，请完成后重试。')
function available() { if (erasing) throw changed() }

/** Erase invalidates results before waiting for native pickers, shares and writes to finish. */
async function run<T>(operation: (current: () => boolean) => Promise<T>): Promise<T> {
  available()
  const version = epoch
  const current = () => !erasing && version === epoch
  const work = operation(current)
  pending.add(work)
  try { const result = await work; if (!current()) throw changed(); return result }
  finally { pending.delete(work) }
}

/** Only cache copies returned by our picker calls are removed; provider and photo-library originals stay put. */
function releasePicker(uri: string) {
  const file = new File(uri)
  if (!file.uri.startsWith(cachePrefix) || file.uri.startsWith(selectionPrefix)) return
  const key = `picker:${file.uri}`
  index.set(key, file.uri)
  if (file.exists) file.delete()
  index.remove(key)
}

async function retain(uri: string): Promise<File> {
  available()
  selections.create({ idempotent: true, intermediates: true })
  const file = new File(selections, crypto.randomUUID())
  try { await new File(uri).copy(file); return file }
  catch (cause) {
    try { if (file.exists) file.delete() }
    catch (cleanup) { throw new AggregateError([cause, cleanup], '附件复制失败，且未能清理部分副本。') }
    throw cause
  }
}

function release(uri: string) {
  if (!uri.startsWith(selectionPrefix)) return
  const file = new File(uri)
  if (file.exists) file.delete()
}

const recoveryStorage = {
  read() {
    const uri = index.getString('pending')
    if (!uri || !uri.startsWith(recoveryPrefix)) return null
    const file = new File(uri)
    return file.exists ? file.textSync() : null
  },
  write(value: string | null) {
    available()
    if (value === null) { if (recovery.exists) recovery.delete(); index.remove('pending'); return }
    recovery.create({ idempotent: true, intermediates: true })
    const id = crypto.randomUUID()
    const file = new File(recovery, `${id}.json`)
    const temporary = new File(recovery, `${id}.tmp`)
    temporary.writeSync(value)
    if (temporary.textSync() !== value) throw new Error('恢复文件没能完整保存，尚未提交更改。')
    temporary.moveSync(file)
    if (file.textSync() !== value) throw new Error('恢复文件没能完整保存，尚未提交更改。')
    const previous = index.getString('pending')
    index.set('pending', file.uri)
    if (index.getString('pending') !== file.uri) throw new Error('恢复文件索引没能保存，尚未提交更改。')
    if (previous?.startsWith(recoveryPrefix)) {
      // A failed old-copy cleanup is retried by directory erase; it cannot invalidate the committed copy.
      try { const old = new File(previous); if (old.exists) old.delete() } catch {}
    }
  },
}

/** The earlier writer kept these sealed copies directly in this app's private Documents directory. */
function rootRecoveryCopy(file: File) {
  if (!/^vibeface-recovery-[A-F0-9]{8}-\d{4}-(0[1-9]|1[0-2])-(0[1-9]|[12]\d|3[01])\.json$/.test(file.name) || file.size < 1 || file.size > 4 * 1024 * 1024) return false
  try {
    const bundle = record(JSON.parse(file.textSync()))
    const envelope = record(bundle.envelope), header = record(envelope.header)
    return bundle.version === 1 && typeof bundle.network_id === 'string' && /^[a-f0-9]{64}$/.test(bundle.network_id)
      && header.format === 'vibeface-recovery' && header.version === 1 && header.kdf === 'argon2id'
      && header.memory_kib === 19_456 && header.iterations === 2 && header.parallelism === 1 && header.cipher === 'xchacha20poly1305'
      && typeof header.salt === 'string' && /^[a-f0-9]{32}$/.test(header.salt)
      && typeof header.nonce === 'string' && /^[a-f0-9]{48}$/.test(header.nonce)
      && typeof envelope.ciphertext === 'string' && envelope.ciphertext.length >= 32 && envelope.ciphertext.length % 2 === 0 && /^[a-f0-9]+$/.test(envelope.ciphertext)
  } catch { return false }
}

async function erase(clear: () => Promise<void>) {
  available()
  erasing = true
  epoch++
  try {
    await Promise.allSettled([...pending])
    for (const key of index.getAllKeys().filter(key => key.startsWith('picker:'))) {
      const uri = index.getString(key)
      if (uri) releasePicker(uri)
    }
    for (const item of Paths.document.list()) if (item instanceof File && rootRecoveryCopy(item)) item.delete()
    for (const directory of [recovery, transfers, selections, drafts, attachments]) if (directory.exists) directory.delete()
    index.clearAll()
    await clear()
  } finally { erasing = false }
}

const draftStorage = {
  get: (key: string) => index.getString(`draft:${key}`),
  set(key: string, value: string) { available(); index.set(`draft:${key}`, value) },
  remove(key: string) { available(); index.remove(`draft:${key}`) },
  all: () => index.getAllKeys().filter(key => key.startsWith('draft:')).map(key => index.getString(key) ?? ''),
}

export const privateFiles = { draftStorage, drafts, attachments, run, retain, release, releasePicker, recoveryStorage, erase, recovery, transfers }
