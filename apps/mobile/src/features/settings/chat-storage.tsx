import { HardDrive, Paperclip } from 'lucide-react-native'
import { useState } from 'react'
import { View } from 'react-native'
import { SettingsGroup, SettingsRow } from '@/components/kit'
import { Button } from '@/components/arc/button'
import { Text } from '@/components/arc/text'
import { ErrorText } from '@/components/action'
import { useWorkspace } from '@/data/context'
import { localConversations } from '@/data/cached'
import { attachmentCacheStats, clearAttachmentCache } from '@/platform/native-chat-attachments'
import { useRun } from '@/lib/use-run'

const size = (bytes: number) => bytes >= 1024 * 1024 ? `${(bytes / 1024 / 1024).toFixed(1)} MiB` : `${Math.ceil(bytes / 1024)} KiB`
export function ChatStorage() {
  const workspace = useWorkspace()
  const [, refresh] = useState(0)
  const scope = { networkId: workspace.networkId ?? '', deviceId: workspace.deviceId ?? '' }
  const stats = localConversations.stats(scope)
  const [media, setMedia] = useState<ReturnType<typeof attachmentCacheStats> | null>(() => { try { return attachmentCacheStats() } catch { return null } })
  const { busy, error, run } = useRun()
  if (!scope.networkId || !scope.deviceId) return null
  return <SettingsGroup title="存储">
    <SettingsRow icon={HardDrive} label="本地聊天记录" description={`${stats.conversations} 段会话 · ${size(stats.bytes)}；最多保留 ${size(stats.maxBytes)}。`} />
    <View className="gap-2 px-4 py-3"><Text className="text-sm text-muted">清除当前网络的离线聊天副本，草稿和待发消息保留。联网后可以重新读取。</Text><Button variant="secondary" disabled={busy !== null || !stats.conversations} onPress={() => void run('clear-history-cache', async () => { localConversations.clear(scope); refresh((value) => value + 1) })}><Text>清除聊天缓存</Text></Button></View>
    <SettingsRow icon={Paperclip} label="已下载附件" description={media ? `${media.files} 个文件 · ${size(media.bytes)}` : '暂时无法读取附件缓存容量。'} />
    <View className="gap-2 px-4 py-3"><Text className="text-sm text-muted">清除本机下载副本后，附件需要联网重新打开。未发送的草稿附件保留。</Text><Button variant="secondary" disabled={busy !== null || media?.files === 0} onPress={() => void run('clear-media-cache', async () => { await clearAttachmentCache(); setMedia(attachmentCacheStats()) })}><Text>清除附件缓存</Text></Button></View>
    {error ? <View className="px-4 pb-3"><ErrorText error={error} /></View> : null}
  </SettingsGroup>
}
