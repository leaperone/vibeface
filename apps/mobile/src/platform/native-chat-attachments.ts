import { Directory, File } from 'expo-file-system'
import * as Sharing from 'expo-sharing'
import type { Attachment } from '@vibeface/client/chat/types.ts'
import { attachmentImageType, downloadAttachment, validAttachment, verifyAttachment, type ReadAttachment } from './chat-attachments'
import { privateFiles } from './files'

export type LocalAttachment = { uri: string; mimeType: string; image: boolean }
function location(scope: string, attachment: Attachment) {
  if (!/^[a-f0-9]{64}$/.test(scope) || !validAttachment(attachment)) throw new Error('附件记录无效，无法打开。')
  const directory = new Directory(privateFiles.attachments, scope, attachment.hash)
  return { directory, file: new File(directory, attachment.name) }
}
/** Scope must hash network + device + conversation; cached bytes never cross that boundary. */
export async function localAttachment(scope: string, attachment: Attachment, read?: ReadAttachment, progress?: (received: number) => void): Promise<LocalAttachment> {
  return privateFiles.run(async current => {
    const { directory, file } = location(scope, attachment)
    let bytes: Uint8Array | undefined
    if (file.exists) {
      try { bytes = await verifyAttachment(attachment, await file.bytes()) }
      catch { if (file.exists) file.delete() }
    }
    if (!bytes) {
      if (!read) throw new Error('附件尚未下载，请连接原网络后重试。')
      bytes = await downloadAttachment(attachment, async (file, offset) => {
        if (!current()) throw new Error('应用内数据正在清除。')
        return read(file, offset)
      }, progress)
      if (!current()) throw new Error('应用内数据正在清除。')
      directory.create({ idempotent: true, intermediates: true })
      const temporary = new File(directory, `${crypto.randomUUID()}.tmp`)
      try {
        await temporary.write(bytes)
        await verifyAttachment(attachment, await temporary.bytes())
        if (!current()) throw new Error('应用内数据正在清除。')
        if (file.exists) file.delete()
        await temporary.move(file)
      } finally { if (temporary.exists) temporary.delete() }
    }
    const mimeType = attachmentImageType(bytes)
    return { uri: file.uri, mimeType: mimeType ?? 'application/octet-stream', image: mimeType !== null }
  })
}

export async function shareAttachment(local: LocalAttachment, title: string) {
  await privateFiles.run(async current => {
    if (!new File(local.uri).exists) throw new Error('本地副本已被清除，请重新下载。')
    if (!await Sharing.isAvailableAsync()) throw new Error('这个设备暂时无法打开系统分享，请稍后重试。')
    if (current()) await Sharing.shareAsync(local.uri, { mimeType: local.mimeType, dialogTitle: title })
  })
}
