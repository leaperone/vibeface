import { useEffect, useRef, useState, useSyncExternalStore, type ReactNode } from 'react'
import { useNavigate } from '@tanstack/react-router'
import { ArchiveRestore, CircleCheck, Download, FileKey, KeyRound, LockKeyhole, ShieldAlert, type LucideIcon } from 'lucide-react'
import { cn } from 'cn'
import { ActionCard, BusyIcon, Details, FilePicker, InlineError, Panel, SettingsRow } from '@/components/app/kit'
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

/** Another browser or device used a recovery file; this one asks whether that was the person. */
export function RecoveredCard() {
  const { recoverySince, acknowledgeRecovery } = useDevice()
  const navigate = useNavigate()
  return <ActionCard icon={ShieldAlert} title="有设备刚用了恢复文件" actions={<>
    <Button type="button" variant="ghost" onClick={() => { acknowledgeRecovery(); void navigate({ to: '/devices' }) }}>不是我</Button>
    <Button type="button" onClick={acknowledgeRecovery}>是我</Button>
  </>}>
    {recoverySince ? <time className="text-muted-foreground tabular-nums" dateTime={new Date(recoverySince).toISOString()}>{new Date(recoverySince).toLocaleString('zh-CN', { dateStyle: 'long', timeStyle: 'short' })}</time> : null}
  </ActionCard>
}

/** Shared by creating and recovering: the sealed file, then the promise it is kept apart from its password. */
export function SaveRecoveryFile({ file, saved, onSaved, download, busy = false }: {
  file: string; saved: boolean; onSaved(saved: boolean): void; download?: { done: boolean; run(): void }; busy?: boolean
}) {
  const downloaded = !download || download.done
  return <>
    <p className="flex items-center gap-2 text-muted-foreground"><LockKeyhole aria-hidden="true" className="size-4 shrink-0" />文件用恢复密码加密，没有密码打不开</p>
    {downloaded ? <p className="flex min-h-11 min-w-0 items-center gap-2 rounded-lg bg-muted/60 ps-3 text-meta">
      <CircleCheck aria-hidden="true" className="size-4 shrink-0 text-success" /><span className="sr-only">已下载 </span>
      <span className="min-w-0 flex-1 py-2 font-mono [overflow-wrap:anywhere]">{file}</span>
      {download ? <Button type="button" variant="ghost" disabled={busy} onClick={download.run}>重新下载</Button> : null}
    </p> : <Button type="button" className="w-fit" disabled={busy} onClick={download.run}><Download aria-hidden="true" />下载恢复文件</Button>}
    <label className={cn('flex min-h-11 items-center gap-2', !downloaded && 'text-muted-foreground')}><input type="checkbox" className="size-4" disabled={!downloaded || busy} checked={saved} onChange={(event) => onSaved(event.target.checked)} />已把文件和密码分开保存</label>
  </>
}

// Kept in the tab's session so a reload before confirming still lands on saving the new file.
const recoveredKey = 'vibeface.recovered'
let recovered = typeof sessionStorage !== 'undefined' && sessionStorage.getItem(recoveredKey) === '1'
const recoveredListeners = new Set<() => void>()
function markRecovered(value: boolean) {
  recovered = value
  if (value) sessionStorage.setItem(recoveredKey, '1'); else sessionStorage.removeItem(recoveredKey)
  for (const listener of recoveredListeners) listener()
}
/** Whether a recovery just finished here and its new file still waits to be confirmed as saved. */
export function useJustRecovered() {
  return useSyncExternalStore((listener) => { recoveredListeners.add(listener); return () => { recoveredListeners.delete(listener) } }, () => recovered, () => recovered)
}

/** After a recovery, before anything else: the new file was already downloaded, unasked, so it gets confirmed here. */
export function RecoveredStep() {
  const { directory } = useDevice()
  const code = useRecoveryCode(directory?.recovery_key)
  const [saved, setSaved] = useState(false)
  return <Panel icon={FileKey} title="保存新的恢复文件">
    <p className="flex items-center gap-2 text-success"><CircleCheck aria-hidden="true" className="size-4 shrink-0" />已恢复，旧文件已失效</p>
    <SaveRecoveryFile file={code ? recoveryFileStem(code) : ''} saved={saved} onSaved={setSaved} />
    <Button type="button" className="w-fit justify-self-end" disabled={!saved} onClick={() => markRecovered(false)}>完成</Button>
  </Panel>
}

/** After unlocking a recovery file: keep every device except the lost ones. */
export function RecoveryReview() {
  const device = useDevice()
  const review = device.review
  const { busy, error, run } = useGenerationRun()
  const [lost, setLost] = useState<string[]>([])
  if (review?.kind !== 'recovery') return null
  return <section className="grid gap-4 rounded-xl border bg-card shadow-xs p-5">
    <h2 className="flex items-center gap-2 text-heading font-semibold"><ArchiveRestore aria-hidden="true" className="size-5 text-muted-foreground" />哪台设备丢了？</h2>
    <fieldset disabled={busy} className="grid gap-2">
      {[...review.members].sort((a, b) => Number(b.device_id === review.requiredNode) - Number(a.device_id === review.requiredNode)).map((member) => {
        const required = member.device_id === review.requiredNode
        const Icon = deviceIcon({ kind: member.kind, name: member.name, entry: required })
        const gone = lost.includes(member.device_id)
        return <div key={member.device_id} className="flex min-h-12 items-center gap-2 rounded-lg bg-muted/60 px-3">
          <Icon aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" />
          <span className="grid min-w-0 flex-1 py-1.5">
            <span className={cn('truncate', gone && 'text-muted-foreground line-through')}>{member.name}</span>
            {gone ? <span className="text-meta text-danger">将被移出，不能再连接</span> : null}
          </span>
          {required ? <span className="text-meta text-muted-foreground">当前连接</span>
            : <label className="-me-3 flex min-h-11 cursor-pointer items-center gap-2 px-3 text-meta">丢了<input type="checkbox" role="switch" className="switch" data-tone="danger" checked={gone} onChange={(event) => setLost(event.target.checked ? [...lost, member.device_id] : lost.filter((id) => id !== member.device_id))} /></label>}
        </div>
      })}
    </fieldset>
    <p className="text-meta text-muted-foreground">恢复后会下载新的恢复文件，旧文件失效</p>
    <InlineError error={error || null} />
    <div className="flex flex-wrap justify-between gap-2">
      <Button type="button" variant="ghost" onClick={device.cancelSensitive}>取消</Button>
      <Button type="button" disabled={busy} onClick={() => void run(async () => {
        // Set before the call: connecting again ends this review, and the connect page then asks for the new file first.
        markRecovered(true)
        try {
          await device.confirmRecovery(review.members.filter((member) => member.device_id === review.requiredNode || !lost.includes(member.device_id)))
        } catch (cause) { markRecovered(false); throw cause }
      })}><BusyIcon busy={busy} />恢复</Button>
    </div>
    {busy ? <p className="text-meta text-muted-foreground">如果已经提交，取消不会撤回；重新连接后看一下设备列表。</p> : null}
  </section>
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
