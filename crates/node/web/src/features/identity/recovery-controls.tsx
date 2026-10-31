import { useEffect, useMemo, useRef, useState, type ReactNode } from 'react'
import { Link, useNavigate } from '@tanstack/react-router'
import { ArchiveRestore, BellRing, CalendarClock, ChevronRight, CircleCheck, CircleMinus, CirclePlus, Copy, Download, FileKey, FileWarning, Hand, Hourglass, KeyRound, Laptop, LifeBuoy, ScanLine, Shield, ShieldAlert, ShieldCheck, ShieldX, Smartphone, WifiOff, type LucideIcon } from 'lucide-react'
import qrcode from 'qrcode-generator'
import { cn } from '@/lib/utils'
import { ActionCard, BusyIcon, claimFocus, Details, FilePicker, InlineError, Panel, PasswordInput, SettingsDisclosure, SettingsRow, toast, WaitingDot } from '@/components/app/kit'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { entryAddress, messageOf, parseEntry } from '@vibeface/client/lib/node.ts'
import { passphrase, passwordStrength, recoveryFileStem } from '@vibeface/client/lib/secure.ts'
import { defaultBrowserName } from '@/lib/device-name'
import { useDevice, useRecoveryCode } from '@vibeface/client/identity/context.tsx'
import { passwordCopy, recoveryChanges, recoveryCopy, securityCopy, whenText } from '@vibeface/client/identity/copy.ts'
import { recoveryEntry, recoveryLink } from '@vibeface/client/identity/recovery-link.ts'
import { deviceIcon } from '@/features/devices/devices'

/** Where the reminder to change the recovery password leads: settings with that form open. */
const PASSWORD_HASH = 'recovery-password'

function field(form: HTMLFormElement, name: string) { const element = form.elements.namedItem(name); return element instanceof HTMLInputElement || element instanceof HTMLTextAreaElement ? element.value : '' }
/** A failed try keeps the chosen file and name; only typed passwords leave the page. */
function clearPasswords(form: HTMLFormElement) { for (const input of form.querySelectorAll<HTMLInputElement>('input[name$=password]')) input.value = '' }
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
  return { busy: busyGeneration === generation, error, status, run, clear: () => setError('') }
}

/** The file that still works: the code its name carries; the day it was set shows under 安全. */
export function CurrentRecoveryFile() {
  const { directory } = useDevice()
  const code = useRecoveryCode(directory?.recovery_key)
  if (!code) return null
  return <SettingsRow icon={FileKey} label={recoveryCopy.file} description={<span title={recoveryFileStem(code)}>{recoveryCopy.codeLabel} <span className="font-medium tracking-wide text-foreground">{code}</span></span>} />
}

/**
 * Under a new recovery password: an icon and a word for how hard it is to guess, and why a weak one is refused.
 * `tried`: a submit was refused, so a too-short one turns red too.
 */
export function PasswordStrength({ id, password, tried = false }: { id: string; password: string; tried?: boolean }) {
  const strength = password ? passwordStrength(password) : null
  if (!strength || strength.level === 'weak' && strength.problem === 'short' && !tried) return <span id={id} className={cn('text-meta font-normal', tried ? 'text-danger' : 'text-muted-foreground')}>{passwordCopy.short}</span>
  const Icon = strength.level === 'weak' ? ShieldX : strength.level === 'strong' ? ShieldCheck : Shield
  return <span id={id} className={cn('flex items-center gap-1.5 text-meta font-normal', strength.level === 'weak' ? 'text-danger' : strength.level === 'strong' ? 'text-success' : 'text-muted-foreground')}>
    <Icon aria-hidden="true" className="size-4 shrink-0" />{passwordCopy.label[strength.level]}{strength.level === 'weak' ? <span className="min-w-0">· {passwordCopy[strength.problem]}</span> : null}
  </span>
}

/** What a phone scans in its 用恢复文件找回 to know which computer to go through; it holds no key and lets no one in. */
export function PhoneRecovery() {
  const device = useDevice()
  const entry = recoveryEntry(device.entrances, device.ready?.entry ?? null)
  const link = entry ? recoveryLink(entry) : null
  const image = useMemo(() => {
    if (!link) return null
    try { const code = qrcode(0, 'M'); code.addData(link); code.make(); return code.createDataURL(4, 16) } catch { return null }
  }, [link])
  const [copied, setCopied] = useState(false)
  useEffect(() => {
    if (!copied) return
    const timer = setTimeout(() => setCopied(false), 2000)
    return () => clearTimeout(timer)
  }, [copied])
  return <SettingsDisclosure icon={Smartphone} label={recoveryCopy.byPhone}>
    {link ? <>
      {image ? <img src={image} alt={recoveryCopy.qr} className="size-48 rounded-lg bg-white p-2 ring-1 ring-border" /> : null}
      <p className="flex items-center gap-2 text-meta text-muted-foreground"><ScanLine aria-hidden="true" className="size-4 shrink-0" />{recoveryCopy.scanHint}</p>
      <Button type="button" variant="outline" size="sm" className="w-fit" onClick={() => { void navigator.clipboard.writeText(link).then(() => setCopied(true), () => {}) }}>
        {copied ? <CircleCheck aria-hidden="true" className="text-success" /> : <Copy aria-hidden="true" />}{copied ? '已复制' : '复制链接'}
      </Button>
    </> : <p className="flex items-center gap-2 text-meta text-muted-foreground"><WifiOff aria-hidden="true" className="size-4 shrink-0" />{recoveryCopy.unreachable}</p>}
  </SettingsDisclosure>
}

/** Someone is recovering the network with a recovery file; a manager here can stop it before it takes effect. */
export function RecoveryPendingCard() {
  const device = useDevice()
  const { busy, error, run } = useGenerationRun()
  // Both manager decisions are irreversible, so each takes one confirmation, as on the phone.
  const [confirming, setConfirming] = useState<'confirm' | 'stop' | null>(null)
  const pending = device.pendingRecovery
  if (!pending) return null
  const changes = recoveryChanges(pending, device.directory?.members ?? [])
  return <ActionCard icon={ShieldAlert} tone="danger" title={recoveryCopy.pending} meta={recoveryCopy.effectiveIn(pending.effective_at_ms - device.nodeClockOffset)} actions={confirming || busy ? <>
    {/* Remount the confirmation row so focus lands on 取消 and a second press cannot confirm. */}
    <Button key={`cancel-${confirming ?? 'busy'}`} type="button" variant="ghost" autoFocus disabled={busy} onClick={() => setConfirming(null)}>取消</Button>
    <Button key={confirming ?? 'busy'} type="button" variant={confirming === 'stop' ? 'destructive' : 'default'} className={confirming === 'stop' ? 'bg-destructive text-primary-foreground shadow-xs hover:bg-destructive/88 focus-visible:border-ring focus-visible:ring-ring/50 dark:bg-destructive dark:hover:bg-destructive/88 dark:focus-visible:ring-ring/50' : undefined} disabled={busy} aria-busy={busy || undefined} onClick={() => void run(async () => {
      if (confirming === 'confirm') {
        await (device as typeof device & { approveRecovery(checkpoint: string): Promise<void> }).approveRecovery(pending.checkpoint)
        toast(recoveryCopy.confirmDone, { tone: 'success' })
      } else {
        await device.vetoRecovery(pending.checkpoint)
        toast(recoveryCopy.stopDone, { tone: 'success' })
      }
      setConfirming(null)
    })}><BusyIcon busy={busy} icon={confirming === 'stop' ? Hand : ShieldCheck} />{confirming === 'stop' ? recoveryCopy.stop : recoveryCopy.confirm}</Button>
  </> : <>
    <Button type="button" onClick={() => setConfirming('confirm')}><ShieldCheck aria-hidden="true" />{recoveryCopy.confirm}</Button>
    <Button type="button" variant="ghost" onClick={() => { device.dismissPendingRecovery(); toast(recoveryCopy.itsMeDone(pending.effective_at_ms - device.nodeClockOffset), { icon: Hourglass }) }}>{recoveryCopy.itsMe}</Button>
    <Button type="button" variant="destructive" onClick={() => setConfirming('stop')}><Hand aria-hidden="true" />{recoveryCopy.stop}</Button>
  </>}>
    {confirming || busy ? <div role="alert" className="grid gap-1 rounded-lg bg-danger-soft px-3 py-2">
      <p className="font-medium">{confirming === 'stop' ? recoveryCopy.stopAsk : recoveryCopy.confirmAsk}</p>
      <p className="text-meta text-muted-foreground">{confirming === 'stop' ? recoveryCopy.stopNote : recoveryCopy.confirmNote}</p>
    </div> : changes.some(({ added }) => !added) ? <p className="text-meta text-muted-foreground">{recoveryCopy.outIfNotStopped}</p> : null}
    {changes.length ? <ul className="grid gap-1">{changes.map(({ member, added }) => {
      const Icon = deviceIcon(member)
      const Change = added ? CirclePlus : CircleMinus
      return <li key={member.device_id} className="flex min-w-0 items-center gap-2">
        <Change role="img" aria-label={added ? '加入' : '移出'} className={cn('size-4 shrink-0', added ? 'text-warning' : 'text-danger')} />
        <Icon aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" />
        <span className={cn('truncate', !added && 'text-muted-foreground line-through')}>{member.name}</span>
        {added ? <span className="shrink-0 rounded-md bg-warning-soft px-1.5 text-meta text-warning">{recoveryCopy.added}</span>
          : member.device_id === device.identity?.deviceId ? <span className="shrink-0 rounded-md bg-muted px-1.5 text-meta text-muted-foreground">这个浏览器</span> : null}
      </li>
    })}</ul> : null}
    <InlineError error={error || null} />
  </ActionCard>
}

/** Atop every page while a recovery waits: a manager sees it without opening 待处理, and one tap leads there. */
export function RecoveryAlert() {
  const device = useDevice()
  const pending = device.pendingRecovery
  if (!pending) return null
  return <div className="mx-auto w-full max-w-(--page-width) shrink-0 px-4 pt-3 sm:px-6">
    <Link to="/inbox" className="flex min-h-11 min-w-0 items-center gap-2.5 rounded-xl border border-danger-border bg-card py-1.5 ps-1.5 pe-3 shadow-xs transition-colors hover:bg-danger-soft">
      <span className="grid size-8 shrink-0 place-items-center rounded-lg bg-danger-soft text-danger ring-1 ring-danger-border"><ShieldAlert aria-hidden="true" className="size-4" /></span>
      <span className="min-w-0 flex-1 truncate font-medium">{recoveryCopy.pending}</span>
      <span className="shrink-0 text-meta text-danger tabular-nums">{recoveryCopy.effectiveIn(pending.effective_at_ms - device.nodeClockOffset)}</span>
      <ChevronRight aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" />
    </Link>
  </div>
}

/** Left after stopping a recovery: whoever started it holds the file, so a new password retires it. */
export function RotateHintCard() {
  const device = useDevice()
  const navigate = useNavigate()
  return <ActionCard icon={ShieldAlert} title={recoveryCopy.leaked} actions={<>
    <Button type="button" variant="ghost" onClick={device.dismissRotateHint}>{recoveryCopy.later}</Button>
    <Button type="button" onClick={() => void navigate({ to: '/settings', hash: PASSWORD_HASH })}>{recoveryCopy.rotate}</Button>
  </>}>
    <p className="flex items-center gap-2 text-meta text-muted-foreground"><Hand aria-hidden="true" className="size-4 shrink-0" />{recoveryCopy.leakedNote}</p>
  </ActionCard>
}

/** Shared by creating and recovering: the sealed file, then the promise it is kept apart from its password. */
export function SaveRecoveryFile({ code, saved, onSaved, download, busy = false }: {
  code: string; saved: boolean; onSaved(saved: boolean): void; download?: { done: boolean; run(): void }; busy?: boolean
}) {
  const downloaded = !download || download.done
  // The download button gives way to the file once it is down, and is disabled while it works; ticking the box is next.
  const box = useRef<HTMLInputElement>(null)
  const settled = downloaded && !busy
  const wasSettled = useRef(settled)
  useEffect(() => { if (settled && !wasSettled.current) claimFocus(box.current); wasSettled.current = settled }, [settled])
  return <>
    <p className="flex items-center gap-2 text-muted-foreground"><LifeBuoy aria-hidden="true" className="size-4 shrink-0" />{recoveryCopy.why}</p>
    <p className="flex items-center gap-2 text-muted-foreground"><FileWarning aria-hidden="true" className="size-4 shrink-0" />{recoveryCopy.onlyCopy}</p>
    {downloaded ? <p className="flex min-h-11 min-w-0 items-center gap-2 rounded-lg bg-muted/60 ps-3 text-meta">
      <CircleCheck aria-hidden="true" className="size-4 shrink-0 text-success" /><span className="sr-only">已下载 </span>
      {/* The code is what tells files apart; on a narrow screen the shared prefix gives way first. */}
      <span title={code ? recoveryFileStem(code) : undefined} className="flex min-w-0 flex-1 py-2 font-mono whitespace-nowrap">
        <span className="truncate text-muted-foreground">{recoveryFileStem('')}</span><span className="font-medium tracking-wide">{code}</span>
      </span>
      {download ? <Button type="button" variant="ghost" disabled={busy} onClick={download.run}>重新下载</Button> : null}
    </p> : <Button type="button" className="w-fit" disabled={busy} onClick={download.run}><Download aria-hidden="true" />下载恢复文件</Button>}
    <label className={cn('flex min-h-11 items-center gap-2', !downloaded && 'text-muted-foreground')}><input ref={box} type="checkbox" className="size-4" disabled={!downloaded || busy} checked={saved} onChange={(event) => onSaved(event.target.checked)} />{recoveryCopy.keptApart}</label>
  </>
}

/**
 * A new recovery file this page made and still holds: downloaded once on its own, offered again from a tap until the
 * person confirms it is kept apart from its password, which drops the copy.
 * `landing`: where the focus goes once confirming takes the file, and the box ticked, off the page.
 */
export function UnsavedRecoveryFile({ landing }: { landing(): HTMLElement | null | undefined }) {
  const { unsavedRecovery: file, downloadUnsavedRecovery, forgetUnsavedRecovery } = useDevice()
  const [error, setError] = useState('')
  if (!file) return null
  return <>
    <SaveRecoveryFile code={file.code} saved={false} onSaved={(saved) => { if (saved) { landing()?.focus({ preventScroll: true }); forgetUnsavedRecovery() } }}
      download={{ done: file.attempted, run: () => { setError(''); downloadUnsavedRecovery().catch((cause) => setError(messageOf(cause))) } }} />
    <InlineError error={error || null} />
  </>
}

/** This browser's recovery while it waits on the Node, or once one of the devices it would replace stopped it. */
export function RecoveryWaiting() {
  const { recovery, dismissRecovery, unsavedRecovery, forgetUnsavedRecovery } = useDevice()
  const saving = useRef<HTMLElement>(null)
  if (recovery?.status === 'stopped') return <Panel icon={ShieldX} title={recoveryCopy.stopped}>
    <p className="text-muted-foreground">{recoveryCopy.stoppedNote}</p>
    <Button type="button" className="w-fit justify-self-end" onClick={() => { forgetUnsavedRecovery(); dismissRecovery() }}>知道了</Button>
  </Panel>
  const at = recovery?.status === 'waiting' ? recovery.effectiveAt : null
  return <Panel icon={Hourglass} title={recoveryCopy.waiting}>
    {at === null ? null : <>
      <p role="status" className="flex items-center gap-2 text-heading font-semibold tabular-nums"><WaitingDot />{recoveryCopy.left(at)}</p>
      <p className="flex items-center gap-2 text-muted-foreground"><CalendarClock aria-hidden="true" className="size-4 shrink-0" /><time className="tabular-nums" dateTime={new Date(at).toISOString()}>{whenText(at)}</time>{recoveryCopy.autoEnter}</p>
    </>}
    <p className="flex items-center gap-2 text-muted-foreground"><BellRing aria-hidden="true" className="size-4 shrink-0" />{recoveryCopy.managersSee}</p>
    <p className="flex items-center gap-2 text-muted-foreground"><Laptop aria-hidden="true" className="size-4 shrink-0" />{recoveryCopy.closeOk}</p>
    {/* Saving the new file is a separate thing to do now, not part of the wait. */}
    {unsavedRecovery ? <section ref={saving} className="grid gap-3 border-t pt-4">
      <h3 className="flex items-center gap-2 font-medium"><FileKey aria-hidden="true" className="size-4 shrink-0" />{recoveryCopy.saveNew}</h3>
      <p className="flex items-center gap-2 text-meta text-muted-foreground"><KeyRound aria-hidden="true" className="size-4 shrink-0" />{recoveryCopy.samePassword}</p>
      <UnsavedRecoveryFile landing={() => saving.current?.parentElement?.querySelector('h2')} />
    </section> : null}
  </Panel>
}

/** After a recovery took effect, before anything else: its new file was downloaded unasked, so it gets confirmed here. */
export function RecoveredStep() {
  const { directory, finishRecovered, unsavedRecovery: file, downloadUnsavedRecovery, forgetUnsavedRecovery } = useDevice()
  const code = useRecoveryCode(directory?.recovery_key)
  const [saved, setSaved] = useState(false)
  const [error, setError] = useState('')
  // Ticking 已分开保存 drops the copy this page held for another download.
  return <Panel icon={FileKey} title={recoveryCopy.saveNew}>
    <p className="flex items-center gap-2 text-success"><CircleCheck aria-hidden="true" className="size-4 shrink-0" />{recoveryCopy.recovered}</p>
    <SaveRecoveryFile code={code ?? ''} saved={saved} onSaved={(value) => { setSaved(value); if (value) forgetUnsavedRecovery() }}
      download={file ? { done: file.attempted, run: () => { setError(''); downloadUnsavedRecovery().catch((cause) => setError(messageOf(cause))) } } : undefined} />
    <InlineError error={error || null} />
    <Button type="button" className="w-fit justify-self-end" disabled={!saved} onClick={() => { forgetUnsavedRecovery(); finishRecovered() }}>完成</Button>
  </Panel>
}

/** After unlocking a recovery file: keep every device except the lost ones. */
export function RecoveryReview() {
  const device = useDevice()
  const review = device.review
  const { busy, error, run } = useGenerationRun()
  const [lost, setLost] = useState<string[]>([])
  if (review?.kind !== 'recovery') return null
  return <Panel icon={ArchiveRestore} title={recoveryCopy.whichLost}>
    <fieldset disabled={busy} className="grid gap-2">
      {[...review.members].sort((a, b) => Number(b.device_id === review.requiredNode) - Number(a.device_id === review.requiredNode)).map((member) => {
        const required = member.device_id === review.requiredNode
        const Icon = deviceIcon(member)
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
    <p className="flex items-center gap-2 text-meta text-muted-foreground"><Hourglass aria-hidden="true" className="size-4 shrink-0" />{recoveryCopy.delay}</p>
    <p className="flex items-center gap-2 text-meta text-muted-foreground"><Download aria-hidden="true" className="size-4 shrink-0" />会下载新的恢复文件，生效后旧文件作废</p>
    <InlineError error={error || null} />
    <div className="flex flex-wrap justify-between gap-2">
      <Button type="button" variant="ghost" className="-ms-3" onClick={device.cancelSensitive}>取消</Button>
      <Button type="button" disabled={busy} onClick={() => void run(() => device.confirmRecovery(review.members.filter((member) => member.device_id === review.requiredNode || !lost.includes(member.device_id))))}><BusyIcon busy={busy} />找回</Button>
    </div>
    {busy ? <p className="text-meta text-muted-foreground">{recoveryCopy.cancelLate}</p> : null}
  </Panel>
}

/** Find devices again with the recovery file (connection page only), or re-seal that file with a new password. */
export function RecoveryControls({ only }: { only?: 'restore' | 'passphrase' }) {
  const device = useDevice()
  const localEntry = device.localSetup?.entry
  const target = device.ready?.entry || device.entry || localEntry
  const { busy, error, status, run, clear } = useGenerationRun()
  const [password, setPassword] = useState('')
  const [tried, setTried] = useState(false)
  const [open, setOpen] = useState(only ?? (typeof location !== 'undefined' && location.hash === `#${PASSWORD_HASH}` ? 'passphrase' : undefined))
  // Opened from the reminder: the form may sit below the fold on a phone.
  useEffect(() => { if (open === 'passphrase' && !only) document.getElementById(PASSWORD_HASH)?.scrollIntoView({ block: 'center' }) }, [])
  const row = useRef<HTMLButtonElement>(null)
  const fold = useRef<HTMLElement>(null)
  // Folding the form away takes the focused control with it; its row takes the focus back.
  function close() {
    setOpen(undefined)
    const active = document.activeElement
    if (!active || active === document.body || fold.current?.contains(active)) row.current?.focus({ preventScroll: true })
  }
  if (device.review?.kind === 'recovery') return <RecoveryReview />
  // In settings the password is a row that folds its form out below itself; alone, a form stands as a card.
  const frame = (icon: LucideIcon, title: string, body: ReactNode, id?: string) => only ? <Panel icon={icon} title={title}>{body}</Panel> : <section ref={fold} id={id} className="grid gap-4 px-4 pt-1 pb-4 sm:ps-15">{body}</section>
  const restoreForm = open === 'restore' ? frame(ArchiveRestore, '用恢复文件找回', <>
      <p className="text-meta text-muted-foreground">{recoveryCopy.scope}</p>
      <form className="grid gap-4" noValidate onChange={clear} onSubmit={(event) => {
        event.preventDefault(); const form = event.currentTarget
        void run(async () => {
          let work: Promise<void>
          try {
            if (!field(form, 'address').trim() || !field(form, 'device_id').trim()) throw new Error(`请在“${target ? recoveryCopy.connectTo : recoveryCopy.manual}”里填好要连的电脑。`)
            work = device.beginRecovery({ file: fileOf(form), password: field(form, 'password'), name: field(form, 'name'), entry: parseEntry({ address: field(form, 'address'), device_id: field(form, 'device_id') }) })
          }
          finally { clearPasswords(form) }
          await work
        })
      }}><fieldset disabled={busy || !!device.review} className="grid gap-4">
        <div className="grid gap-1.5 text-meta font-medium">恢复文件<FilePicker name="bundle" accept="application/json,.json" required /></div>
        <Label label="恢复密码"><PasswordInput name="password" autoComplete="off" required /></Label>
        <Label label="这个浏览器叫什么？"><Input name="name" required maxLength={128} defaultValue={defaultBrowserName()} /></Label>
        {/* With no address yet, opening this computer's own page fills it in; typing it is the fallback. */}
        {target ? null : <p className="flex items-center gap-2 text-meta text-muted-foreground"><Laptop aria-hidden="true" className="size-4 shrink-0" />在要连的电脑上打开本机页面再找回</p>}
        <Details label={target ? recoveryCopy.connectTo : recoveryCopy.manual}>
          <Label label="访问地址"><Input type="url" name="address" required defaultValue={target ? entryAddress(target) : ''} /></Label>
          <Label label="设备编号"><Input className="font-mono" name="device_id" required pattern="[a-f0-9]{64}" defaultValue={target?.device_id || ''} autoComplete="off" spellCheck={false} /></Label>
        </Details>
        <InlineError error={error || null} />
        <div className="flex flex-wrap justify-end gap-2"><Button type="submit" aria-busy={busy || undefined}><BusyIcon busy={busy} />下一步</Button></div>
      </fieldset></form>
    </>) : null
  const passphraseForm = open === 'passphrase' ? frame(KeyRound, '换恢复密码', <>
      <p className="text-meta text-muted-foreground">{recoveryCopy.replaceHint}</p>
      {/* A failed try keeps what was typed; a change to any field takes the old error away. */}
      <form className="grid gap-4" noValidate onChange={clear} onSubmit={(event) => {
        event.preventDefault(); const form = event.currentTarget
        void run(async () => {
          const file = fileOf(form)
          if (!field(form, 'old_password')) throw new Error('请输入旧密码。')
          setTried(true)
          await device.replaceRecoveryFile({ file, oldPassword: field(form, 'old_password'), password: passphrase(field(form, 'password')) })
          clearPasswords(form); setPassword(''); setTried(false)
          if (!only) close()
        }, recoveryCopy.replaced)
      }}><fieldset disabled={busy || !!device.review} className="grid gap-4">
        <div className="grid gap-1.5 text-meta font-medium">恢复文件<FilePicker name="bundle" accept="application/json,.json" required /></div>
        <Label label="旧密码"><PasswordInput name="old_password" autoComplete="off" required /></Label>
        <div className="grid gap-1.5">
          <Label label="新密码"><PasswordInput name="password" autoComplete="new-password" required aria-describedby="new-password-strength" onChange={(event) => setPassword(event.target.value)} /></Label>
          <PasswordStrength id="new-password-strength" password={password} tried={tried} />
        </div>
        <InlineError error={error || null} />
        <div className="flex flex-wrap justify-end gap-2">{!only ? <Button type="button" variant="ghost" onClick={close}>取消</Button> : null}<Button type="submit" aria-busy={busy || undefined}><BusyIcon busy={busy} icon={Download} />换密码并下载新文件</Button></div>
      </fieldset></form>
    </>, PASSWORD_HASH) : null
  const done = status ? <p role="status" className={only ? 'text-meta text-success' : 'px-4 py-3 text-meta text-success'}>{status}</p> : null
  const unsaved = device.unsavedRecovery && !device.recovered ? <section className="grid gap-3 px-4 pt-1 pb-4 sm:ps-15"><UnsavedRecoveryFile landing={() => row.current} /></section> : null
  if (only) return <div className="grid gap-4">{restoreForm}{passphraseForm}{done}</div>
  // Finding devices again lives on the connection page: here it would start a real two-day recovery against this person's own devices.
  return <>
    <SettingsRow ref={row} icon={KeyRound} label={securityCopy.password} expanded={open === 'passphrase'} className={open === 'passphrase' ? 'border-b-0' : undefined} onClick={() => setOpen(open === 'passphrase' ? undefined : 'passphrase')}>
      {device.recoverySince ? <time className="tabular-nums" dateTime={new Date(device.recoverySince).toISOString()}>{securityCopy.changedOn(device.recoverySince)}</time> : null}
    </SettingsRow>
    {passphraseForm}
    {done}
    {unsaved}
  </>
}
