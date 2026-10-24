import { useEffect, useRef, useState, type ReactNode } from 'react'
import { useNavigate } from '@tanstack/react-router'
import { ArchiveRestore, CircleCheck, Download, FileKey, Hand, Hourglass, KeyRound, LockKeyhole, ShieldAlert, ShieldX, type LucideIcon } from 'lucide-react'
import { cn } from 'cn'
import { ActionCard, BusyIcon, Details, FilePicker, InlineError, Panel, SettingsRow, toast, WaitingDot } from '@/components/app/kit'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { messageOf, parseEntry } from '@vibeface/client/lib/node.ts'
import { passphrase, recoveryCode, recoveryFileStem } from '@vibeface/client/lib/secure.ts'
import { defaultBrowserName } from '@/lib/device-name'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { deviceIcon } from '@/features/devices/devices'

function field(form: HTMLFormElement, name: string) { const element = form.elements.namedItem(name); return element instanceof HTMLInputElement || element instanceof HTMLTextAreaElement ? element.value : '' }
/** A failed try keeps the chosen file and name; only typed passwords leave the page. */
function clearPasswords(form: HTMLFormElement) { for (const input of form.querySelectorAll<HTMLInputElement>('input[type=password]')) input.value = '' }
function fileOf(form: HTMLFormElement) { const element = form.elements.namedItem('bundle'); const file = element instanceof HTMLInputElement ? element.files?.[0] : null; if (!file) throw new Error('请选择恢复文件。'); return file }
const Label = ({ label, children }: { label: string; children: ReactNode }) => <label className="grid gap-1.5 text-meta font-medium">{label}{children}</label>

/** Busy and error state bound to the connection generation, so a stale result never unlocks a newer action. */
function useGenerationRun() {
  const device = useDevice()
  const generation = device.connectionGeneration()
  const [busyGeneration, setBusyGeneration] = useState<number | null>(null)
  const pending = useRef<number | null>(null)
  const [error, setError] = useState('')
  const [status, setStatus] = useState('')
  useEffect(() => { setError(''); setStatus('') }, [generation])
  async function run(action: () => Promise<void>, success = '') {
    if (pending.current === device.connectionGeneration()) return
    let owner = device.connectionGeneration()
    setError(''); setStatus('')
    try {
      const work = action()
      owner = device.connectionGeneration(); pending.current = owner; setBusyGeneration(owner)
      await work
      if (owner === device.connectionGeneration()) setStatus(success)
    } catch (cause) { if (owner === device.connectionGeneration()) setError(messageOf(cause)) }
    finally { if (pending.current === owner) pending.current = null; setBusyGeneration((active) => active === owner ? null : active) }
  }
  return { busy: busyGeneration === generation, error, status, run }
}

/** The code in the name of the file that opens with `key`, once worked out. */
function useRecoveryCode(key: string | undefined) {
  const [code, setCode] = useState<{ key: string; value: string } | null>(null)
  useEffect(() => {
    if (!key) return
    let live = true
    void recoveryCode(key).then((value) => { if (live) setCode({ key, value }) })
    return () => { live = false }
  }, [key])
  return key && code?.key === key ? code.value : null
}

/** The file that still works: the code in its name and the day it was made. */
export function CurrentRecoveryFile() {
  const { directory, recoverySince } = useDevice()
  const code = useRecoveryCode(directory?.recovery_key)
  if (!code) return null
  return <SettingsRow icon={FileKey} label="恢复文件" description={recoverySince ? <time className="tabular-nums" dateTime={new Date(recoverySince).toISOString()}>{new Date(recoverySince).toLocaleDateString('zh-CN', { dateStyle: 'long' })}</time> : undefined}>
    <code title={recoveryFileStem(code)} className="rounded-md bg-muted px-1.5 py-0.5 font-mono text-meta text-foreground"><span className="sr-only">{recoveryFileStem("")}</span>{code}</code>
  </SettingsRow>
}

/** Hours until `at`, at least one while it has not passed. */
export const hoursUntil = (at: number) => Math.max(1, Math.ceil((at - Date.now()) / 3_600_000))

/** Someone is recovering the network with a recovery file; a manager here can stop it before it takes effect. */
export function RecoveryPendingCard() {
  const device = useDevice()
  const navigate = useNavigate()
  const { busy, error, run } = useGenerationRun()
  const pending = device.pendingRecovery
  if (!pending) return null
  const known = new Set(device.directory?.members.map((member) => member.device_id))
  return <ActionCard icon={ShieldAlert} tone="danger" title="有人正在用恢复文件找回" meta={`${hoursUntil(pending.effective_at_ms)} 小时后生效`} actions={<>
    <Button type="button" variant="ghost" disabled={busy} onClick={device.dismissPendingRecovery}>是我</Button>
    <Button type="button" variant="destructive" disabled={busy} aria-busy={busy || undefined} onClick={() => void run(async () => {
      await device.vetoRecovery()
      toast('已拦下。恢复文件可能泄露了，换个恢复密码更稳妥', { tone: 'success', action: { label: '去换', onClick: () => void navigate({ to: '/settings' }) } })
    })}><BusyIcon busy={busy} icon={Hand} />拦下</Button>
  </>}>
    {pending.members.filter((member) => !known.has(member.device_id)).map((member) => {
      const Icon = deviceIcon({ kind: member.kind, name: member.name, entry: false })
      return <p key={member.device_id} className="flex min-w-0 items-center gap-2"><Icon aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" /><span className="truncate">{member.name}</span></p>
    })}
    <InlineError error={error || null} />
  </ActionCard>
}

/** Shared by creating and recovering: the sealed file, then the promise it is kept apart from its password. */
export function SaveRecoveryFile({ code, saved, onSaved, download, busy = false }: {
  code: string; saved: boolean; onSaved(saved: boolean): void; download?: { done: boolean; run(): void }; busy?: boolean
}) {
  const downloaded = !download || download.done
  return <>
    <p className="flex items-center gap-2 text-muted-foreground"><LockKeyhole aria-hidden="true" className="size-4 shrink-0" />文件用恢复密码加密，没有密码打不开</p>
    {downloaded ? <p className="flex min-h-11 min-w-0 items-center gap-2 rounded-lg bg-muted/60 ps-3 text-meta">
      <CircleCheck aria-hidden="true" className="size-4 shrink-0 text-success" /><span className="sr-only">已下载 </span>
      {/* The code is what tells files apart; on a narrow screen the shared prefix gives way first. */}
      <span title={code ? recoveryFileStem(code) : undefined} className="flex min-w-0 flex-1 py-2 font-mono whitespace-nowrap">
        <span className="truncate text-muted-foreground">{recoveryFileStem('')}</span><span className="font-medium tracking-wide">{code}</span>
      </span>
      {download ? <Button type="button" variant="ghost" disabled={busy} onClick={download.run}>重新下载</Button> : null}
    </p> : <Button type="button" className="w-fit" disabled={busy} onClick={download.run}><Download aria-hidden="true" />下载恢复文件</Button>}
    <label className={cn('flex min-h-11 items-center gap-2', !downloaded && 'text-muted-foreground')}><input type="checkbox" className="size-4" disabled={!downloaded || busy} checked={saved} onChange={(event) => onSaved(event.target.checked)} />已把文件和密码分开保存</label>
  </>
}

/** This browser's recovery while it waits on the Node, or once one of the devices it would replace stopped it. */
export function RecoveryWaiting() {
  const { recovery, dismissRecovery } = useDevice()
  if (recovery?.status === 'stopped') return <Panel icon={ShieldX} title="这次找回被拦下了">
    <p className="text-muted-foreground">新文件用不了，原来的恢复文件还能用</p>
    <Button type="button" className="w-fit justify-self-end" onClick={dismissRecovery}>知道了</Button>
  </Panel>
  const at = recovery?.status === 'waiting' ? recovery.effectiveAt : null
  return <Panel icon={Hourglass} title="等待生效">
    {at === null ? null : <p role="status" className="flex items-center gap-2 text-heading font-semibold tabular-nums"><WaitingDot />{at > Date.now() ? `剩余 ${hoursUntil(at)} 小时` : '马上生效'}</p>}
    <p className="text-muted-foreground">你的其他设备可以拦下</p>
  </Panel>
}

/** After a recovery took effect, before anything else: its new file was downloaded unasked, so it gets confirmed here. */
export function RecoveredStep() {
  const { directory, finishRecovered } = useDevice()
  const code = useRecoveryCode(directory?.recovery_key)
  const [saved, setSaved] = useState(false)
  return <Panel icon={FileKey} title="保存新的恢复文件">
    <p className="flex items-center gap-2 text-success"><CircleCheck aria-hidden="true" className="size-4 shrink-0" />已找回，旧文件已作废</p>
    <SaveRecoveryFile code={code ?? ''} saved={saved} onSaved={setSaved} />
    <Button type="button" className="w-fit justify-self-end" disabled={!saved} onClick={finishRecovered}>完成</Button>
  </Panel>
}

/** After unlocking a recovery file: keep every device except the lost ones. */
export function RecoveryReview() {
  const device = useDevice()
  const review = device.review
  const { busy, error, run } = useGenerationRun()
  const [lost, setLost] = useState<string[]>([])
  if (review?.kind !== 'recovery') return null
  return <Panel icon={ArchiveRestore} title="哪台设备丢了？">
    <fieldset disabled={busy} className="grid gap-2">
      {[...review.members].sort((a, b) => Number(b.device_id === review.requiredNode) - Number(a.device_id === review.requiredNode)).map((member) => {
        const required = member.device_id === review.requiredNode
        const Icon = deviceIcon({ kind: member.kind, name: member.name, entry: required })
        const gone = lost.includes(member.device_id)
        return <div key={member.device_id} className="flex min-h-12 items-center gap-2 rounded-lg bg-muted/60 px-3">
          <Icon aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" />
          <span className="grid min-w-0 flex-1 py-1.5">
            <span className={cn('truncate', gone && 'text-muted-foreground line-through')}>{member.name}</span>
            {gone ? <span className="text-meta text-danger">生效后移出</span> : null}
          </span>
          {required ? <span className="text-meta text-muted-foreground">当前连接</span>
            : <label className="-me-3 flex min-h-11 cursor-pointer items-center gap-2 px-3 text-meta">丢了<input type="checkbox" role="switch" className="switch" data-tone="danger" checked={gone} onChange={(event) => setLost(event.target.checked ? [...lost, member.device_id] : lost.filter((id) => id !== member.device_id))} /></label>}
        </div>
      })}
    </fieldset>
    <p className="flex items-center gap-2 text-meta text-muted-foreground"><Hourglass aria-hidden="true" className="size-4 shrink-0" />2 天后生效，期间你的其他设备可以拦下</p>
    <p className="flex items-center gap-2 text-meta text-muted-foreground"><Download aria-hidden="true" className="size-4 shrink-0" />会下载新的恢复文件，生效后旧文件作废</p>
    <InlineError error={error || null} />
    <div className="flex flex-wrap justify-between gap-2">
      <Button type="button" variant="ghost" className="-ms-3" onClick={device.cancelSensitive}>取消</Button>
      <Button type="button" disabled={busy} onClick={() => void run(() => device.confirmRecovery(review.members.filter((member) => member.device_id === review.requiredNode || !lost.includes(member.device_id))))}><BusyIcon busy={busy} />恢复</Button>
    </div>
    {busy ? <p className="text-meta text-muted-foreground">如果已经提交，取消不会撤回。</p> : null}
  </Panel>
}

/** Find devices again with the recovery file, or re-seal that file with a new password. */
export function RecoveryControls({ only }: { only?: 'restore' | 'passphrase' }) {
  const device = useDevice()
  const localEntry = device.localSetup?.entry
  const target = device.ready?.entry || device.entry || localEntry
  const { busy, error, status, run } = useGenerationRun()
  const [open, setOpen] = useState(only)
  if (device.review?.kind === 'recovery') return <RecoveryReview />
  // In settings each action is a row that folds its form out below itself; alone, the form stands as a card.
  const frame = (icon: LucideIcon, title: string, body: ReactNode) => only ? <Panel icon={icon} title={title}>{body}</Panel> : <section className="grid gap-4 px-4 pt-1 pb-5 sm:ps-15">{body}</section>
  const toggle = (mode: 'restore' | 'passphrase') => setOpen(open === mode ? undefined : mode)
  const restoreForm = open === 'restore' ? frame(ArchiveRestore, '用恢复文件找回', <>
      <p className="text-meta text-muted-foreground">只找回设备和管理权限，不含对话</p>
      <form className="grid gap-4" noValidate onSubmit={(event) => {
        event.preventDefault(); const form = event.currentTarget
        void run(async () => {
          let work: Promise<void>
          try { work = device.beginRecovery({ file: fileOf(form), password: field(form, 'password'), name: field(form, 'name'), entry: parseEntry({ address: field(form, 'address'), device_id: field(form, 'device_id') }) }) }
          finally { clearPasswords(form) }
          await work
        })
      }}><fieldset disabled={busy || !!device.review} className="grid gap-4">
        <div className="grid gap-1.5 text-meta font-medium">恢复文件<FilePicker name="bundle" accept="application/json,.json" required /></div>
        <Label label="恢复密码"><Input type="password" name="password" autoComplete="off" required /></Label>
        <Label label="这个浏览器叫什么？"><Input name="name" required maxLength={128} defaultValue={defaultBrowserName(device.identity?.deviceId)} /></Label>
        <Details label="连接到哪台电脑" open={!target}>
          <Label label="访问地址"><Input type="url" name="address" required defaultValue={target?.address || ''} /></Label>
          <Label label="设备编号"><Input className="font-mono" name="device_id" required pattern="[a-f0-9]{64}" defaultValue={target?.device_id || ''} autoComplete="off" spellCheck={false} /></Label>
        </Details>
        <InlineError error={error || null} />
        <div className="flex flex-wrap justify-end gap-2">{!only ? <Button type="button" variant="ghost" onClick={() => setOpen(undefined)}>取消</Button> : null}<Button type="submit" aria-busy={busy || undefined}><BusyIcon busy={busy} />下一步</Button></div>
      </fieldset></form>
    </>) : null
  const passphraseForm = open === 'passphrase' ? frame(KeyRound, '换恢复密码', <>
      <p className="text-meta text-muted-foreground">换完后旧文件失效</p>
      <form className="grid gap-4" noValidate onSubmit={(event) => {
        event.preventDefault(); const form = event.currentTarget
        void run(async () => {
          let work: Promise<void>
          try { work = device.replaceRecoveryFile({ file: fileOf(form), oldPassword: field(form, 'old_password'), password: passphrase(field(form, 'password')) }) }
          finally { clearPasswords(form) }
          await work
        }, '已下载新文件，旧恢复文件已失效')
      }}><fieldset disabled={busy || !!device.review} className="grid gap-4">
        <div className="grid gap-1.5 text-meta font-medium">恢复文件<FilePicker name="bundle" accept="application/json,.json" required /></div>
        <Label label="旧密码"><Input type="password" name="old_password" autoComplete="off" required /></Label>
        <Label label="新密码（至少 8 个字符）"><Input type="password" name="password" minLength={8} autoComplete="new-password" required /></Label>
        <InlineError error={error || null} />
        <div className="flex flex-wrap justify-end gap-2">{!only ? <Button type="button" variant="ghost" onClick={() => setOpen(undefined)}>取消</Button> : null}<Button type="submit" aria-busy={busy || undefined}><BusyIcon busy={busy} icon={Download} />下载新文件</Button></div>
      </fieldset></form>
    </>) : null
  const done = status ? <p role="status" className={only ? 'text-meta text-success' : 'px-4 py-3 text-meta text-success'}>{status}</p> : null
  if (only) return <div className="grid gap-4">{restoreForm}{passphraseForm}{done}</div>
  return <>
    <SettingsRow icon={ArchiveRestore} label="用恢复文件找回" expanded={open === 'restore'} className={open === 'restore' ? 'border-b-0' : undefined} onClick={() => toggle('restore')} />
    {restoreForm}
    <SettingsRow icon={KeyRound} label="换恢复密码" expanded={open === 'passphrase'} className={open === 'passphrase' ? 'border-b-0' : undefined} onClick={() => toggle('passphrase')} />
    {passphraseForm}
    {done}
  </>
}
