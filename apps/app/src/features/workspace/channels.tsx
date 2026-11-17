import { EmptyState,InlineError,useAction } from '@/components/arc/product-kit'
import { Link,useNavigate } from '@tanstack/react-router'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { useWorkspace } from '@vibeface/client/workspace/context.tsx'
import { newId } from '@vibeface/client/workspace/storage.ts'
import type { WorkspaceChannel,WorkspaceCommand } from '@vibeface/client/workspace/types.ts'
import { Button } from '@vibeface/uiarc/button/button.tsx'
import { Checkbox } from "@vibeface/uiarc/checkbox/checkbox.tsx"
import { Drawer,DrawerContent } from "@vibeface/uiarc/drawer/drawer.tsx"
import { Input } from '@vibeface/uiarc/input/input.tsx'
import { Select } from "@vibeface/uiarc/select/select.tsx"
import { Textarea } from '@vibeface/uiarc/textarea/textarea.tsx'
import { MessagesSquare,Plus } from 'lucide-react'
import { useRef,useState } from 'react'

export function ChannelsOverview() {
  const workspace = useWorkspace()
  const [creating, setCreating] = useState(false)
  const channels = workspace.workspaceState?.channels ?? []
  return <div className="mx-auto grid w-full max-w-(--page-width) gap-5">
    <div className="flex flex-wrap items-center justify-between gap-3"><div className="grid gap-1"><h2 className="text-lg font-medium">与我的 Bot 协作</h2><p className="text-sm text-secondary">让我的多个 Bot 围绕同一件事协作。</p></div><Button type="button" disabled={!workspace.writable || !workspace.bots.length} onClick={() => setCreating(true)}><Plus aria-hidden="true" />新频道</Button></div>
    {channels.length ? <ul className="grid gap-3">{channels.map((channel) => <li key={channel.channel_id}><Link to="/channels/$channelId" params={{ channelId: channel.channel_id }} className="grid gap-2 rounded-panel border bg-surface p-4 [overflow-wrap:anywhere] transition-colors hover:bg-surface-muted/50  "><span className="flex items-start gap-2 font-medium"><MessagesSquare aria-hidden="true" className="size-5 shrink-0 text-secondary" /><span className="min-w-0 flex-1">{channel.title}</span></span>{channel.purpose ? <span className="text-sm text-secondary">{channel.purpose}</span> : null}<span className="text-sm text-secondary">{channel.members.length} 个 Bot · 默认协调 {workspace.bots.find((bot) => bot.bot_id === channel.coordinator_bot)?.name ?? 'Bot 未加载'}</span></Link></li>)}</ul> : <EmptyState icon={MessagesSquare} title={workspace.workspaceState ? '还没有频道' : '正在读取频道'} action={workspace.workspaceState && workspace.bots.length ? <Button type="button" variant="secondary" disabled={!workspace.writable} onClick={() => setCreating(true)}>新频道</Button> : undefined} />}
    {creating ? <ChannelForm open onOpenChange={setCreating} /> : null}
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
  const opener = useRef<HTMLElement | null>(null)
  const leaving = useRef(false)
  const [titleError, setTitleError] = useState('')
  const titleInput = useRef<HTMLInputElement>(null)
  return <Drawer open={open} onOpenChange={onOpenChange}><DrawerContent title={channel ? '频道设置' : '新频道'} description="普通发言只交给选中的 Bot。没有指定目标时，由默认协调 Bot 承接。" className="overflow-y-auto" onOpenAutoFocus={() => { opener.current = document.activeElement instanceof HTMLElement ? document.activeElement : null }} onCloseAutoFocus={(event) => { event.preventDefault(); if (!leaving.current && opener.current?.isConnected) opener.current.focus({ preventScroll: true }) }}>
    <form className="grid gap-4" noValidate onSubmit={(event) => {
      event.preventDefault()
      if (!submission.current && !title.trim()) { setTitleError('还没填频道名称'); titleInput.current?.focus(); return }
      setTitleError('')
      void action.run('save-channel', async () => {
      if (!members.length || !members.includes(coordinator)) throw new Error('请选择频道成员，并将默认协调 Bot 加入成员。')
      const configuration = { channel_id: channelId.current, title: title.trim(), purpose: purpose.trim(), members, coordinator_bot: coordinator, coordinator_node: node }
      const command: Extract<WorkspaceCommand, { kind: 'create_channel' | 'configure_channel' }> = submission.current ?? (channel ? { kind: 'configure_channel', ...configuration, expected_revision: channel.revision, owner_epoch: channel.owner_epoch } : { kind: 'create_channel', ...configuration })
      submission.current = command
      await workspace.workspaceCommand(command)
      leaving.current = true
      onOpenChange(false)
      await navigate({ to: '/channels/$channelId', params: { channelId: channelId.current } })
    }) }}>
      <div className="grid gap-2 text-sm font-medium"><Input ref={titleInput} label="频道名称" error={titleError} required readOnly={Boolean(submission.current)} value={title} maxLength={512} onChange={(event) => { setTitle(event.currentTarget.value); setTitleError('') }} placeholder="例如：官网改版" /></div>
      <div className="grid gap-2 text-sm font-medium"><Textarea label="频道目标" readOnly={Boolean(submission.current)} value={purpose} maxLength={8192} onChange={(event) => setPurpose(event.currentTarget.value)} placeholder="说明这个频道要完成什么" /></div>
      <fieldset className="grid gap-2"><legend className="mb-2 text-sm font-medium">Bot 成员</legend>{workspace.bots.map((bot) => <label key={bot.bot_id} className="flex min-h-11 items-center gap-3 rounded-control border px-3 py-2 text-sm"><Checkbox aria-label={bot.name} disabled={Boolean(submission.current)} checked={members.includes(bot.bot_id)} onCheckedChange={next => { const checked = next === true;  setMembers((current) => checked ? [...current, bot.bot_id] : current.filter((id) => id !== bot.bot_id))  }} />{bot.name}</label>)}</fieldset>
      <div className="grid gap-2 text-sm font-medium"><Select label="默认协调 Bot" disabled={Boolean(submission.current)} value={"arc:" + coordinator} onValueChange={selected => { const value = selected.slice(4);  setCoordinator(value)  }} options={workspace.bots.filter((bot) => members.includes(bot.bot_id)).map((bot) => ({ value: "arc:" + bot.bot_id, label: bot.name }))} /></div>
      <div className="grid gap-2 text-sm font-medium"><Select label="协调设备" disabled={Boolean(submission.current)} value={"arc:" + node} onValueChange={selected => { const value = selected.slice(4);  setNode(value)  }} options={workspace.nodes.filter((item) => !item.revoked).map((item) => ({ value: "arc:" + item.device_id, label: `${item.name}${item.online ? '' : ' · 离线'}` }))} /></div>
      <InlineError error={action.error} />
      <Button type="submit" disabled={Boolean(action.busy) || !workspace.writable}>{action.busy ? '正在保存' : channel ? '保存频道' : '创建频道'}</Button>
    </form>
  </DrawerContent></Drawer>
}
