import { useRef, useState } from 'react'
import { Link, useNavigate } from '@tanstack/react-router'
import { MessagesSquare, Plus } from 'lucide-react'
import { EmptyState, InlineError, useAction } from '@/components/app/kit'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Textarea } from '@/components/ui/textarea'
import { NativeSelect, NativeSelectOption } from '@/components/ui/native-select'
import { Sheet, SheetContent, SheetDescription, SheetHeader, SheetTitle } from '@/components/ui/sheet'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { useWorkspace } from '@vibeface/client/workspace/context.tsx'
import { newId } from '@vibeface/client/workspace/storage.ts'
import type { WorkspaceChannel, WorkspaceCommand } from '@vibeface/client/workspace/types.ts'

export function ChannelsOverview() {
  const workspace = useWorkspace()
  const [creating, setCreating] = useState(false)
  const channels = workspace.workspaceState?.channels ?? []
  return <div className="mx-auto grid w-full max-w-(--page-width) gap-5">
    <div className="flex flex-wrap items-center justify-between gap-3"><div className="grid gap-1"><h2 className="text-heading font-semibold">与我的 Bot 协作</h2><p className="text-meta text-muted-foreground">让我的多个 Bot 围绕同一件事协作。</p></div><Button type="button" disabled={!workspace.writable || !workspace.bots.length} onClick={() => setCreating(true)}><Plus aria-hidden="true" />新频道</Button></div>
    {channels.length ? <ul className="grid gap-3">{channels.map((channel) => <li key={channel.channel_id}><Link to="/channels/$channelId" params={{ channelId: channel.channel_id }} className="grid gap-2 rounded-xl border bg-card p-4 transition-colors hover:bg-muted/50 focus-visible:ring-3 focus-visible:ring-ring/50"><span className="flex items-center gap-2 font-medium"><MessagesSquare aria-hidden="true" className="size-5 text-muted-foreground" />{channel.title}</span>{channel.purpose ? <span className="text-meta text-muted-foreground">{channel.purpose}</span> : null}<span className="text-meta text-muted-foreground">{channel.members.length} 个 Bot · 默认协调 {workspace.bots.find((bot) => bot.bot_id === channel.coordinator_bot)?.name ?? 'Bot 未加载'}</span></Link></li>)}</ul> : <EmptyState icon={MessagesSquare} title={workspace.workspaceState ? '还没有频道' : '正在读取频道'} action={workspace.workspaceState && workspace.bots.length ? <Button type="button" variant="outline" disabled={!workspace.writable} onClick={() => setCreating(true)}>新频道</Button> : undefined} />}
    <ChannelForm open={creating} onOpenChange={setCreating} />
  </div>
}

export function ChannelForm({ open, onOpenChange, channel }: { open: boolean; onOpenChange: (open: boolean) => void; channel?: WorkspaceChannel }) {
  const workspace = useWorkspace()
  const { ready } = useDevice()
  const navigate = useNavigate()
  const action = useAction()
  const [title, setTitle] = useState(channel?.title ?? '')
  const [purpose, setPurpose] = useState(channel?.purpose ?? '')
  const [members, setMembers] = useState(channel?.members ?? workspace.bots.map((bot) => bot.bot_id))
  const [coordinator, setCoordinator] = useState(channel?.coordinator_bot ?? workspace.selectedBot?.bot_id ?? workspace.bots[0]?.bot_id ?? '')
  const [node, setNode] = useState(channel?.coordinator_node ?? ready?.entry.device_id ?? '')
  const channelId = useRef(channel?.channel_id ?? newId())
  const submission = useRef<Extract<WorkspaceCommand, { kind: 'create_channel' | 'configure_channel' }> | null>(null)
  return <Sheet open={open} onOpenChange={onOpenChange}><SheetContent className="overflow-y-auto"><SheetHeader><SheetTitle>{channel ? '频道设置' : '新频道'}</SheetTitle><SheetDescription>普通发言只交给选中的 Bot。没有指定目标时，由默认协调 Bot 承接。</SheetDescription></SheetHeader>
    <form className="grid gap-4 px-4 pb-6" onSubmit={(event) => { event.preventDefault(); void action.run('save-channel', async () => {
      if (!members.length || !members.includes(coordinator)) throw new Error('请选择频道成员，并将默认协调 Bot 加入成员。')
      const configuration = { channel_id: channelId.current, title: title.trim(), purpose: purpose.trim(), members, coordinator_bot: coordinator, coordinator_node: node }
      const command: Extract<WorkspaceCommand, { kind: 'create_channel' | 'configure_channel' }> = submission.current ?? (channel ? { kind: 'configure_channel', ...configuration, expected_revision: channel.revision, owner_epoch: channel.owner_epoch } : { kind: 'create_channel', ...configuration })
      submission.current = command
      await workspace.workspaceCommand(command)
      onOpenChange(false)
      await navigate({ to: '/channels/$channelId', params: { channelId: channelId.current } })
    }) }}>
      <label className="grid gap-2 text-meta font-medium">频道名称<Input required readOnly={Boolean(submission.current)} value={title} maxLength={512} onChange={(event) => setTitle(event.currentTarget.value)} placeholder="例如：官网改版" /></label>
      <label className="grid gap-2 text-meta font-medium">频道目标<Textarea readOnly={Boolean(submission.current)} value={purpose} maxLength={8192} onChange={(event) => setPurpose(event.currentTarget.value)} placeholder="说明这个频道要完成什么" /></label>
      <fieldset className="grid gap-2"><legend className="mb-2 text-meta font-medium">Bot 成员</legend>{workspace.bots.map((bot) => <label key={bot.bot_id} className="flex min-h-11 items-center gap-3 rounded-lg border px-3 py-2 text-meta"><input type="checkbox" disabled={Boolean(submission.current)} className="size-4 accent-primary" checked={members.includes(bot.bot_id)} onChange={(event) => { const checked = event.currentTarget.checked; setMembers((current) => checked ? [...current, bot.bot_id] : current.filter((id) => id !== bot.bot_id)) }} />{bot.name}</label>)}</fieldset>
      <label className="grid gap-2 text-meta font-medium">默认协调 Bot<NativeSelect disabled={Boolean(submission.current)} aria-label="默认协调 Bot" value={coordinator} onChange={(event) => setCoordinator(event.currentTarget.value)}>{workspace.bots.filter((bot) => members.includes(bot.bot_id)).map((bot) => <NativeSelectOption key={bot.bot_id} value={bot.bot_id}>{bot.name}</NativeSelectOption>)}</NativeSelect></label>
      <label className="grid gap-2 text-meta font-medium">协调设备<NativeSelect disabled={Boolean(submission.current)} aria-label="频道协调设备" value={node} onChange={(event) => setNode(event.currentTarget.value)}>{workspace.nodes.filter((item) => !item.revoked).map((item) => <NativeSelectOption key={item.device_id} value={item.device_id}>{item.name}{item.online ? '' : ' · 离线'}</NativeSelectOption>)}</NativeSelect></label>
      <InlineError error={action.error} />
      <Button type="submit" disabled={Boolean(action.busy) || !workspace.writable}>{action.busy ? '正在保存' : channel ? '保存频道' : '创建频道'}</Button>
    </form>
  </SheetContent></Sheet>
}
