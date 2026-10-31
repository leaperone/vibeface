import { useState } from 'react'
import { View } from 'react-native'
import { memoryBase, memoryResultLabel, memorySaved, memorySyncConflictLabel, type MemoryControls } from '@vibeface/client/workspace/memory.ts'
import { memoryConflictLabel, memoryOrigins } from '@vibeface/client/workspace/labels.ts'
import { newId } from '@vibeface/client/workspace/storage.ts'
import type { BotSettings, ContextProjection, MemoryBase, MemoryControlResult } from '@vibeface/client/workspace/types.ts'
import { ErrorText, useAction } from '@/components/action'
import { Details } from '@/components/kit'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Text } from '@/components/ui/text'

export function MemorySummary({ settings, context, memory }: { settings: BotSettings; context?: ContextProjection | null; memory: MemoryControls }) {
  const [draft, setDraft] = useState<{ memory_id: string; base: MemoryBase; text: string; result: MemoryControlResult | null } | null>(null)
  const [confirmDelete, setConfirmDelete] = useState(false)
  const [notice, setNotice] = useState('')
  const { busy, run, error, clear } = useAction()
  const start = (entry?: BotSettings['memory'][number]) => {
    clear(); setNotice(''); setConfirmDelete(false)
    setDraft({ memory_id: entry?.memory_id ?? newId(), base: memoryBase(entry?.metadata ?? null), text: entry?.text ?? '', result: null })
  }
  const save = (after: string | null) => {
    if (!draft) return
    void run('memory', async () => {
      const result = await memory.controlMemory(memory.conversationId, draft.memory_id, draft.base, after)
      setNotice(memoryResultLabel(result))
      if (memorySaved(result)) { setDraft(null); setConfirmDelete(false) }
      else setDraft({ ...draft, result })
    })
  }
  return <View className="gap-3">
    <Text className="text-meta text-muted-foreground">已保存 {settings.memory.length} · 待提交 {settings.memory_staged_count ?? '未提供'} · 提交冲突 {settings.memory_conflicts?.length ?? '未提供'} · 本轮省略 {context?.coverage.memory_omitted ?? '未提供'}</Text>
    <Text className="text-meta text-muted-foreground">待提交和提交冲突来自负责设备最近的观察。Bot 待提交记忆只在本轮成功后保存。由你维护的记忆始终受保护。省略的记忆仍可按需读取。</Text>
    <Button variant="outline" className="self-start" disabled={!memory.writable || busy !== null || draft !== null} onPress={() => start()}><Text>新增记忆</Text></Button>
    {draft ? <View className="gap-3 rounded-xl border border-border p-3">
      <Text className="text-meta font-medium">记忆内容</Text>
      <Input accessibilityLabel="记忆内容" multiline textAlignVertical="top" className="min-h-24" value={draft.text} editable={busy === null} onChangeText={(text) => setDraft({ ...draft, text })} />
      <Text className="text-meta text-muted-foreground">最多 500 个字符。保存后由你维护，Bot 不会自动覆盖或恢复已删除的记忆。</Text>
      {draft.result && !memorySaved(draft.result) ? <View className="gap-2"><Text accessibilityLiveRegion="polite" className="text-meta">{draft.result.current?.text ?? '当前记忆已删除或这台设备尚无记录。'}</Text><Button variant="outline" className="self-start" disabled={busy !== null} onPress={() => { setDraft({ ...draft, base: memoryBase(draft.result?.current?.metadata ?? null), result: null }); setConfirmDelete(false); clear(); setNotice('') }}><Text>按当前版本继续编辑</Text></Button></View> : null}
      <View className="flex-row flex-wrap gap-2">
        <Button disabled={!memory.writable || busy !== null || !draft.text.trim()} onPress={() => save(draft.text)}><Text>{busy ? '正在查明结果…' : '保存记忆'}</Text></Button>
        {draft.base.version !== null ? <Button variant="outline" disabled={!memory.writable || busy !== null} onPress={() => setConfirmDelete(true)}><Text>删除记忆</Text></Button> : null}
        <Button variant="ghost" disabled={busy !== null} onPress={() => { setDraft(null); setConfirmDelete(false); clear() }}><Text>取消编辑</Text></Button>
      </View>
      {confirmDelete ? <View className="gap-2"><Text className="text-meta">删除后 Bot 不会自动恢复这条记忆。</Text><View className="flex-row flex-wrap gap-2"><Button variant="destructive" disabled={!memory.writable || busy !== null} onPress={() => save(null)}><Text>确认删除记忆</Text></Button><Button variant="ghost" disabled={busy !== null} onPress={() => setConfirmDelete(false)}><Text>保留记忆</Text></Button></View></View> : null}
    </View> : null}
    <ErrorText error={error ? `${error} 草稿已保留，重试会先查明原操作结果。` : ''} />
    {notice ? <Text accessibilityLiveRegion="polite" className="text-meta">{notice}</Text> : null}
    <Details label={`查看已保存记忆 · ${settings.memory.length}`}>
      {settings.memory.length ? settings.memory.map((entry) => <View key={entry.memory_id} className="gap-2">
        <Text selectable>{entry.text}</Text>
        <Text className="text-meta text-muted-foreground">{memoryOrigins[entry.metadata.origin]} · {entry.metadata.user_controlled ? '由你维护，受保护' : 'Bot 可更新'} · {entry.metadata.essential ? '优先带入上下文' : '按需读取'}</Text>
        <Details label="记忆版本"><Text selectable className="font-mono text-meta">数据版本 {entry.metadata.epoch} · 维护版本 {entry.metadata.control_revision} · {entry.metadata.revision}</Text><Text selectable className="font-mono text-meta">来源事件 {entry.metadata.source_event_hash}</Text></Details>
        <Button variant="outline" className="self-start" disabled={!memory.writable || busy !== null || draft !== null} onPress={() => start(entry)}><Text>{entry.metadata.user_controlled ? '编辑记忆' : '编辑并接管'}</Text></Button>
      </View>) : <Text className="text-muted-foreground">当前记录没有已保存记忆</Text>}
    </Details>
    {settings.memory_conflicts?.length ? <Details label={`查看记忆冲突 · ${settings.memory_conflicts.length}`}>{settings.memory_conflicts.map((conflict) => <View key={`${conflict.call_ref}:${conflict.memory_id}`} className="gap-1"><Text>{memoryConflictLabel(conflict.reason)}</Text><Text selectable className="font-mono text-meta">记忆 {conflict.memory_id} · 数据版本 {conflict.expected_epoch} / {conflict.actual_epoch}</Text><Text selectable className="font-mono text-meta">预期 {conflict.expected_revision || '尚无版本'} · 当前 {conflict.actual_revision || '已移除'}</Text></View>)}</Details> : null}
    <Details label={`查看同步冲突 · ${settings.memory_sync_conflicts.items.length}${settings.memory_sync_conflicts.has_more ? '+' : ''}`}>
      <Text className="text-meta text-muted-foreground">同步冲突记录共享历史中的其他分支，不代表未保存草稿或本轮待办。</Text>
      {settings.memory_sync_conflicts.items.length ? settings.memory_sync_conflicts.items.map((conflict) => <View key={`${conflict.memory_id}:${conflict.losing_revision}`} className="gap-1"><Text>{memorySyncConflictLabel(conflict.reason)}</Text><Text selectable className="font-mono text-meta">记忆 {conflict.memory_id}</Text><Text selectable className="font-mono text-meta">当前 {conflict.winning_revision} · 其他分支 {conflict.losing_revision}</Text></View>) : <Text className="text-meta text-muted-foreground">当前入口未发现同步冲突</Text>}
      {settings.memory_sync_conflicts.has_more ? <Text className="text-meta text-muted-foreground">仅显示前 50 项同步冲突</Text> : null}
    </Details>
  </View>
}
