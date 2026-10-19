import { useEffect, useRef, useState } from 'react'
import { ChevronDown } from 'lucide-react'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from '@/components/ui/collapsible'
import { Input } from '@/components/ui/input'
import { NativeSelect, NativeSelectOption } from '@/components/ui/native-select'
import { Textarea } from '@/components/ui/textarea'
import { useDevice } from '@/features/identity/context'
import { messageOf, parseNodeSettings } from '@/lib/node'
import type { NodeSettings, NodeSettingsResponse } from '@/lib/node'

type Draft = NodeSettingsResponse['defaults'] & { provider: 'disabled' | 'cursor_sdk' | 'codex_sdk'; workspace: string; model: string; roots: string }
const draftOf = (value: NodeSettingsResponse): Draft => ({
  ...value.defaults, ...value.settings.manager, provider: value.settings.manager?.provider ?? 'disabled',
  workspace: value.settings.manager?.workspace ?? value.defaults.workspace, model: value.settings.manager?.model ?? '', roots: value.settings.roots.join('\n'),
})

export function NodeSettingsControls({ active }: { active: boolean }) {
  const { ready } = useDevice()
  const [response, setResponse] = useState<NodeSettingsResponse | null>(null)
  const [draft, setDraft] = useState<Draft | null>(null)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState('')
  const [status, setStatus] = useState('')
  const sequence = useRef(0)
  const pending = useRef(false)
  useEffect(() => {
    const owner = ++sequence.current
    setResponse(null); setDraft(null); setError(''); setStatus(''); setBusy(false); pending.current = false
    if (!active || !ready?.principal.can_manage) return
    setBusy(true)
    void ready.request('node_settings').then(value => {
      if (owner !== sequence.current || !ready.isCurrent()) return
      const parsed = parseNodeSettings(value)
      setResponse(parsed); setDraft(draftOf(parsed))
    }).catch(cause => {
      if (owner === sequence.current && ready.isCurrent()) setError(messageOf(cause))
    }).finally(() => { if (owner === sequence.current) setBusy(false) })
    return () => { sequence.current += 1 }
  }, [active, ready])
  async function run(save: boolean) {
    if (pending.current || !ready?.isCurrent() || !ready.principal.can_manage || save && !draft) return
    const connection = ready, owner = ++sequence.current
    pending.current = true; setBusy(true); setError(''); setStatus('')
    try {
      let value: unknown
      if (save && draft) {
        const settings: NodeSettings = {
          roots: draft.roots.split('\n').map(value => value.trim()).filter(Boolean),
          manager: draft.provider === 'disabled' ? null : {
            provider: draft.provider, workspace: draft.workspace.trim(), model: draft.model.trim(),
            cursor_sdk_bin: draft.cursor_sdk_bin.trim(), node_bin: draft.node_bin.trim(), codex_sdk_script: draft.codex_sdk_script.trim(),
          },
        }
        value = await connection.request('configure_node', settings)
      } else value = await connection.request('node_settings')
      if (owner !== sequence.current || !connection.isCurrent()) return
      const parsed = parseNodeSettings(value)
      setResponse(parsed); setDraft(draftOf(parsed)); setStatus(save ? '设置已保存，新操作立即生效。正在运行的任务不会重启或自动取消。' : '已读取节点的当前设置。')
    } catch (cause) {
      if (owner === sequence.current && connection.isCurrent()) setError(`${messageOf(cause)} 请重新读取设置后核对。`)
    } finally {
      if (owner === sequence.current) { pending.current = false; setBusy(false) }
    }
  }
  if (!ready?.principal.can_manage) return null
  return <Card>
    <CardHeader><CardTitle>此节点的 Agent 与项目目录</CardTitle></CardHeader>
    <CardContent className="space-y-4">
      <p className="break-all text-sm text-muted-foreground">这些设置只保存在当前节点。{ready.entry.address}</p>
      {error && <p role="alert" className="text-sm text-destructive">{error}</p>}
      <p role="status" className="text-sm text-muted-foreground">{busy ? '正在读取或保存设置…' : status}</p>
      {response && <p className="text-sm text-muted-foreground">{response.readiness.manager === 'disabled' ? '选择 Provider 后保存，即可启用管理 Agent。默认工作目录会自动创建。'
        : response.readiness.manager === 'role_unavailable' ? '此节点没有调度角色授权，当前不能运行管理 Agent。保存配置不会授予新角色。'
        : response.readiness.manager === 'missing_prerequisites' ? `还缺少 ${response.readiness.missing.join('、')}。请先在此节点安装所需程序。`
        : '已检测到所需程序，Provider 登录和真实模型调用尚未验证。'}</p>}
      {response && !response.readiness.executor_available && <p className="text-sm text-muted-foreground">此节点没有执行角色授权。设置项目目录不会启用编码执行。</p>}
      {draft && <form className="space-y-4" onSubmit={event => { event.preventDefault(); void run(true) }}>
        <fieldset disabled={busy} className="space-y-4 disabled:opacity-70">
          <label className="grid gap-2 text-sm">管理 Agent<NativeSelect className="w-full [&>select]:min-h-11" name="provider" value={draft.provider} onChange={event => {
            const provider = event.target.value
            if (provider === 'disabled' || provider === 'cursor_sdk' || provider === 'codex_sdk') setDraft({ ...draft, provider, model: provider === 'cursor_sdk' ? 'auto' : '' })
          }}><NativeSelectOption value="disabled">暂不启用</NativeSelectOption><NativeSelectOption value="codex_sdk">Codex SDK</NativeSelectOption><NativeSelectOption value="cursor_sdk">Cursor SDK</NativeSelectOption></NativeSelect></label>
          {draft.provider !== 'disabled' && <>
            <p className="text-xs text-muted-foreground">Agent 使用独立工作目录，首次保存时自动创建默认目录。项目编辑范围在下方单独设置。</p>
            <label className="grid gap-2 text-sm">模型<Input className="min-h-11" name="model" required={draft.provider === 'cursor_sdk'} value={draft.model} maxLength={256} placeholder={draft.provider === 'cursor_sdk' ? 'auto' : '留空使用 Codex 默认模型'} onChange={event => setDraft({ ...draft, model: event.target.value })} /></label>
            <p className="text-xs text-muted-foreground">请先在此节点安装并登录所选 Provider。此处不保存账户凭证，保存配置不会调用模型。</p>
            <Collapsible>
              <CollapsibleTrigger asChild><Button className="min-h-11 gap-2" type="button" variant="ghost">工作目录与程序路径<ChevronDown className="size-4" /></Button></CollapsibleTrigger>
              <CollapsibleContent forceMount className="space-y-4 pt-3 data-[state=closed]:hidden">
                <label className="grid gap-2 text-sm">Agent 工作目录<Input className="min-h-11" name="workspace" value={draft.workspace} placeholder="/absolute/path/to/agent-workspace" onChange={event => setDraft({ ...draft, workspace: event.target.value })} aria-describedby="manager-workspace-hint" /></label>
                <p id="manager-workspace-hint" className="text-xs text-muted-foreground">默认目录自动创建。自定义目录须已存在，并使用此节点上的绝对路径。</p>
                {draft.provider === 'cursor_sdk' ? <label className="grid gap-2 text-sm">Cursor SDK Bridge<Input className="min-h-11 font-mono text-xs" name="cursor_sdk_bin" value={draft.cursor_sdk_bin} onChange={event => setDraft({ ...draft, cursor_sdk_bin: event.target.value })} /></label> : <>
                  <label className="grid gap-2 text-sm">Node.js<Input className="min-h-11 font-mono text-xs" name="node_bin" value={draft.node_bin} onChange={event => setDraft({ ...draft, node_bin: event.target.value })} /></label>
                  <label className="grid gap-2 text-sm">Codex SDK sidecar<Input className="min-h-11 font-mono text-xs" name="codex_sdk_script" value={draft.codex_sdk_script} onChange={event => setDraft({ ...draft, codex_sdk_script: event.target.value })} /></label>
                </>}
              </CollapsibleContent>
            </Collapsible>
          </>}
          <label className="grid gap-2 text-sm">允许执行的项目目录<Textarea name="roots" className="min-h-28 font-mono text-xs" value={draft.roots} placeholder="/absolute/path/to/project" aria-describedby="execution-roots-hint" onChange={event => setDraft({ ...draft, roots: event.target.value })} /></label>
          <p id="execution-roots-hint" className="text-xs text-muted-foreground">每行填写一个此节点已有的绝对目录。留空时，不允许执行器访问项目目录。</p>
          <Button className="min-h-11" type="submit">保存节点设置</Button>
        </fieldset>
      </form>}
      <Button className="min-h-11" type="button" variant="outline" disabled={busy} onClick={() => void run(false)}>重新读取设置</Button>
    </CardContent>
  </Card>
}
