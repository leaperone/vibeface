import { File } from 'expo-file-system'
import { createChatDrafts } from './chat-drafts'
import { privateFiles } from './files'

export type { ChatDraft, DraftScope, DraftSnapshot } from './chat-drafts'
const prefix = `${privateFiles.drafts.uri.replace(/\/$/, '')}/`

const repository = createChatDrafts({
  storage: privateFiles.draftStorage,
  revision: () => crypto.randomUUID(),
  files: {
    exists: uri => new File(uri).exists,
    owns: uri => uri.startsWith(prefix) && !uri.slice(prefix.length).includes('/'),
    async retain(attachment) {
      return privateFiles.run(async current => {
        privateFiles.drafts.create({ idempotent: true, intermediates: true })
        const copy = new File(privateFiles.drafts, crypto.randomUUID())
        try {
          await new File(attachment.uri).copy(copy)
          if (!current()) throw new Error('应用内数据正在清除，请稍后重试。')
          if (!copy.exists || (attachment.size !== null && copy.size !== attachment.size)) throw new Error('附件没能完整保留，请重新选择。')
          return { uri: copy.uri, name: attachment.name, size: copy.size, mimeType: attachment.mimeType }
        } catch (cause) { if (copy.exists) copy.delete(); throw cause }
      })
    },
    release(uri) { if (uri.startsWith(prefix)) { const file = new File(uri); if (file.exists) file.delete() } },
  },
})

const selectionUsers = new Map<string, number>(), retiredSelections = new Set<string>(), ownedSelections = new Set<string>()
export const chatDrafts = {
  ...repository,
  lease(snapshot: Parameters<typeof repository.lease>[0]) {
    const lease = repository.lease(snapshot)
    let active = true
    return {
      files: lease.files.map((file) => ({
        ...file,
        async arrayBuffer() {
          if (!active) throw new Error('附件读取已结束，请重新发送。')
          return privateFiles.run(async () => {
            const reader = repository.lease({ ...snapshot, files: [file] })
            try { return await new File(file.uri).arrayBuffer() }
            finally { reader.release() }
          })
        },
      })),
      release() { if (active) { active = false; lease.release() } },
    }
  },
  async write(...args: Parameters<typeof repository.write>) {
    const input = new Set(args[1].files.map(file => file.uri))
    for (const file of args[1].files) if ('dispose' in file && typeof file.dispose === 'function') ownedSelections.add(file.uri)
    for (const uri of input) selectionUsers.set(uri, (selectionUsers.get(uri) ?? 0) + 1)
    try {
      const saved = await repository.write(...args)
      for (const uri of input) if (!saved.files.some(next => next.uri === uri)) retiredSelections.add(uri)
      return saved
    } finally {
      for (const uri of input) {
        const count = (selectionUsers.get(uri) ?? 1) - 1
        if (count) selectionUsers.set(uri, count)
        else {
          selectionUsers.delete(uri)
          if (retiredSelections.has(uri)) {
            try {
              if (!ownedSelections.has(uri)) privateFiles.release(uri)
              retiredSelections.delete(uri)
            } catch { /* Erase also clears picker selections. */ }
          }
          ownedSelections.delete(uri)
        }
      }
    }
  },
}

/** Only discard unreferenced private draft copies on a fresh process, before leases can exist. */
if (privateFiles.drafts.exists) {
  const records = privateFiles.draftStorage.all().join('\n')
  for (const file of privateFiles.drafts.list()) if (file instanceof File && !records.includes(JSON.stringify(file.uri))) {
    try { file.delete() } catch { /* Erase retries private-directory cleanup. */ }
  }
}
