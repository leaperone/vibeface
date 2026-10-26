import { useEffect, useRef, useState, type FormEvent, type ReactNode } from 'react'
import { Archive, ArchiveRestore, Camera, CircleCheck, FileKey, Hourglass, KeyRound, Laptop, Link2, LoaderCircle, Monitor, MonitorSmartphone, MonitorX, Pencil, QrCode, RotateCcw, ShieldAlert, ShieldCheck, X } from 'lucide-react'
import { cn } from '@/lib/utils'
import { AuthPage, Details, EmptyState, InlineError, Panel, PasswordInput, claimFocus, SafetyCode, Skeleton, Steps, WaitingDot } from '@/components/app/kit'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Textarea } from '@/components/ui/textarea'
import { addressName, hostOf } from '@vibeface/client/workspace/labels.ts'
import { messageOf, parseEntry } from '@vibeface/client/lib/node.ts'
import { spentInvite } from '@vibeface/client/lib/errors.ts'
import { passphrase, recoveryCode } from '@vibeface/client/lib/secure.ts'
import { defaultBrowserName, defaultComputerName, lastBrowserName, rememberBrowserName } from '@/lib/device-name'
import { startOnboarding } from './onboarding-flag'
import { primaryLane } from '@vibeface/client/identity/connection-lane.ts'
import { notJoined, removedText, useDevice } from '@vibeface/client/identity/context.tsx'
import { webPlatform } from '@/lib/platform'

const NOT_JOINED = notJoined(webPlatform.self)
const BROWSER_REMOVED = removedText(webPlatform.self)
import { RecoveryControls, RecoveryReview, RecoveryWaiting, SaveRecoveryFile } from './recovery-controls'
import { CopyDiagnostics } from './device-controls'
import { joinTicket } from '@vibeface/client/lib/transport.ts'

type Lane = 'local' | 'invite' | 'saved' | 'recovery'
type Purpose = 'empty' | 'link' | 'invalid'
const value = (form: HTMLFormElement, key: string) => { const field = form.elements.namedItem(key); return field instanceof HTMLInputElement || field instanceof HTMLTextAreaElement ? field.value : '' }

function purposeOf(invitation: string): Purpose {
  if (!invitation.trim()) return 'empty'
  return joinTicket(invitation) ? 'link' : 'invalid'
}

type Detector = { detect(source: HTMLVideoElement): Promise<Array<{ rawValue: string }>> }
const detectorClass = () => (globalThis as { BarcodeDetector?: new (options: { formats: string[] }) => Detector }).BarcodeDetector
const canScan = () => typeof navigator !== 'undefined' && Boolean(navigator.mediaDevices?.getUserMedia) && Boolean(detectorClass())

/** Reads a join QR code with the camera; only the ticket is kept, any address in it is ignored. */
function Scanner({ onTicket, onCancel }: { onTicket(ticket: string): void; onCancel(): void }) {
  const video = useRef<HTMLVideoElement>(null)
  const [error, setError] = useState('')
  useEffect(() => {
    let live = true, stream: MediaStream | null = null, timer: ReturnType<typeof setTimeout> | undefined
    const Detector = detectorClass()!
    const detector = new Detector({ formats: ['qr_code'] })
    const tick = async () => {
      if (!live || !video.current) return
      try {
        for (const code of await detector.detect(video.current)) {
          const found = joinTicket(code.rawValue)
          if (found) { onTicket(found.ticket); return }
        }
      } catch {}
      timer = setTimeout(() => void tick(), 250)
    }
    navigator.mediaDevices.getUserMedia({ video: { facingMode: 'environment' }, audio: false }).then(async (media) => {
      if (!live || !video.current) { media.getTracks().forEach(track => track.stop()); return }
      stream = media; video.current.srcObject = media
      await video.current.play()
      void tick()
    }).catch(() => { if (live) setError('没能打开相机，可把链接粘贴到这里') })
    return () => { live = false; clearTimeout(timer); stream?.getTracks().forEach(track => track.stop()) }
  }, [])
  return <div className="grid gap-3">
    {error ? <InlineError error={error} /> : <video ref={video} className="aspect-square w-full rounded-lg bg-black object-cover" playsInline muted aria-label="对准二维码" />}
    <Button type="button" variant="ghost" className="-ms-3 w-fit" onClick={onCancel}>不扫了，粘贴链接</Button>
  </div>
}


function Removed({ busy, run }: { busy: boolean; run: Run }) {
  const device = useDevice()
  const commands = <a href="https://github.com/leaperone/vibeface/blob/main/docs/deployment.md" target="_blank" rel="noreferrer" className="underline">按部署说明选择数据目录并重新加入</a>
  // On its own local page the rejoin first opens this computer, then carries on there.
  const setup = device.localSetup?.kind === 'initialized' && device.localSetup.removed ? device.localSetup : null
  const here = Boolean(setup)
  const canRejoin = device.removedLocally || Boolean(setup?.local_launch)
  return <Panel icon={MonitorX} title={here || device.removedLocally ? '这台电脑已被移出' : '这台设备已被移出'}>
    {canRejoin ? <>
      <p className="text-muted-foreground">点重新加入，再用已有设备的加入链接把它加回来</p>
      <p className="flex items-center gap-2 text-meta text-muted-foreground"><Archive aria-hidden="true" className="size-4 shrink-0" />旧数据改名保留，不会删除</p>
      <Button type="button" className="w-fit justify-self-end" disabled={busy} onClick={() => void run(device.rejoin)}>{busy ? <LoaderCircle aria-hidden="true" className="animate-spin" /> : <RotateCcw aria-hidden="true" />}重新加入</Button>
      <Details label="用命令重新加入">{commands}</Details>
    </> : <>
      {here ? null : <p className="text-muted-foreground">在它的本机页面点“重新加入”。</p>}
      <Details label="用命令重新加入">{commands}</Details>
    </>}
  </Panel>
}

/** The code appears once the device that sent the link shows its own, so until then the next step is over there. */
function JoinCode({ code }: { code?: string }) {
  if (!code) return <p role="status" className="flex items-center gap-2 text-muted-foreground"><LoaderCircle aria-hidden="true" className="size-4 shrink-0 animate-spin" />在发链接的设备上点“显示安全码”</p>
  return <>
    <p role="status" className="flex items-center gap-2 text-muted-foreground"><WaitingDot />两边的安全码一致，就在那边点允许</p>
    <SafetyCode code={code} />
  </>
}

/** The Node's own join: it shows the short code the manager compares before approving. */
function NodeJoin() {
  const device = useDevice()
  const joining = device.nodeJoining!
  const [cancelling, setCancelling] = useState(false)
  return <Panel icon={Hourglass} title={joining.status === 'joined' ? '已加入' : '等待批准'}>
    {joining.status === 'joined' ? <p className="flex items-center gap-2 text-success"><CircleCheck aria-hidden="true" className="size-4 shrink-0" />正在打开这台电脑</p> : <>
      <JoinCode code={joining.code} />
      <Button type="button" variant="ghost" className="-ms-3 w-fit" disabled={cancelling} onClick={() => { setCancelling(true); device.cancelNodeJoin().catch(() => setCancelling(false)) }}>
        {cancelling ? <LoaderCircle aria-hidden="true" className="animate-spin" /> : null}{cancelling ? '正在取消' : '取消'}
      </Button>
    </>}
  </Panel>
}

function Field({ label, children }: { label: string; children: ReactNode }) {
  return <label className="grid gap-1.5 text-meta font-medium">{label}{children}</label>
}

/** A public entry only forwards to people's own devices, so its page only opens join links. */
function usePublicEntry() {
  const [publicEntry, setPublicEntry] = useState(false)
  useEffect(() => {
    let live = true
    if (typeof fetch === 'function') fetch('/api/info', { cache: 'no-store', credentials: 'omit' })
      .then(response => response.ok ? response.json() : null)
      .then(info => { if (live) setPublicEntry(info?.public_entry === true) })
      .catch(() => {})
    return () => { live = false }
  }, [])
  return publicEntry
}

/** Opened from the Home Screen: iOS keeps such an app's storage apart from Safari's. */
const standalone = () => typeof window !== 'undefined' && (window.matchMedia?.('(display-mode: standalone)').matches || (navigator as { standalone?: boolean }).standalone === true)

/** `lane`: the way in to show first, as the join page does. */
export function ConnectionScreen({ lane: firstLane }: { lane?: Lane } = {}) {
  const device = useDevice()
  const publicEntry = usePublicEntry()
  const generation = device.connectionGeneration()
  const [busyGeneration, setBusyGeneration] = useState<number | null>(null)
  const busy = busyGeneration === generation
  const pending = useRef<number | null>(null)
  const [error, setError] = useState('')
  const [notice, setNotice] = useState('')
  const [invitation, setInvitation] = useState(device.initialInvitation)
  const [lane, setLane] = useState<Lane | null>(firstLane ?? null)
  const [names, setNames] = useState<Names | null>(null)
  // Going back from the password keeps what was typed.
  const [draftNames, setDraftNames] = useState<Names | null>(null)
  const [scanning, setScanning] = useState(false)
  const scannable = canScan()
  const savedName = device.localSetup?.kind === 'initialized' ? device.entryName(device.localSetup.entry) : ''
  const computerName = savedName || '这台电脑'
  const removed = device.error === BROWSER_REMOVED
  const suggested = primaryLane({ hasInvitation: Boolean(device.initialInvitation), savedCount: device.entries.length, hasLocalSetup: Boolean(device.localSetup), rejoined: device.rejoined, removed })
  const active = lane ?? suggested
  const purpose = purposeOf(invitation)
  const usable = !!device.identity && !['blocked', 'key_lost', 'initializing', 'removed', 'restarting'].includes(device.phase)
  const thisComputer = device.localSetup?.kind === 'uninitialized'
  const linking = busy || ['connecting', 'reconnecting'].includes(device.phase)
  // A newly opened link replaces whatever is here, including a spent one, and the failure that came with it.
  useEffect(() => { if (device.initialInvitation) { setInvitation(device.initialInvitation); setError(''); setNotice('') } }, [device.initialInvitation])
  useEffect(() => { setError('') }, [generation])
  // A link is single-use: once its request ends without joining, offering 继续 again only leads to "已用过".
  const wasJoining = useRef(false)
  useEffect(() => {
    if (device.joining) wasJoining.current = true
    else if (wasJoining.current) { wasJoining.current = false; if (!device.ready) setInvitation('') }
  }, [device.joining, device.ready])

  function chooseLane(next: Lane) {
    if (!device.entry || next !== 'recovery') device.disconnect()
    setLane(next)
  }
  async function run(action: () => Promise<void>) {
    if (pending.current === device.connectionGeneration()) return
    let owner = device.connectionGeneration()
    setError('')
    try {
      const work = action()
      owner = device.connectionGeneration(); pending.current = owner; setBusyGeneration(owner)
      await work
    } catch (cause) { if (owner === device.connectionGeneration()) setError(messageOf(cause)) }
    finally { if (pending.current === owner) pending.current = null; setBusyGeneration((current) => current === owner ? null : current) }
  }
  function submitInvitation(event: FormEvent<HTMLFormElement>) {
    event.preventDefault()
    const form = event.currentTarget
    setNotice('')
    void run(async () => {
      try {
        if (!thisComputer) { rememberBrowserName(value(form, 'name')); return await device.join(invitation, value(form, 'name')) }
        // The entry spent the invite on the cancelled request, so it cannot be used again.
        if (await device.joinThisComputer(invitation, value(form, 'name')) === 'cancelled') { setInvitation(''); setNotice('已取消。这条链接已用过，要加入请在已有设备上重新添加。') }
      } catch (cause) {
        // A spent link only fails again, so the field opens for a new one.
        if (cause instanceof Error && spentInvite.test(cause.message)) setInvitation('')
        throw cause
      }
    })
  }

  // Not being, or no longer being, one of my devices is a state to explain, not an error.
  const membership = device.error === NOT_JOINED || device.error === BROWSER_REMOVED ? device.error : ''
  const deviceError = membership ? '' : device.error
  const quiet = notice || (!error ? membership : '')
  const failed = Boolean(error || deviceError && device.phase !== 'reconnecting')
  const alert = failed ? <InlineError error={error || deviceError} />
    : quiet ? <p role="status" className="flex items-center gap-2 rounded-lg bg-muted/60 px-3 py-2 text-meta text-muted-foreground">{quiet === membership ? <MonitorX aria-hidden="true" className="size-4 shrink-0" /> : null}{quiet}</p> : null
  const focused = Boolean(device.review || device.joining || device.nodeJoining || device.recovery)
  const setup = device.localSetup?.kind === 'uninitialized'
  const step = active === 'local' && setup && !focused ? (names ? 2 : 1) : device.review?.kind === 'bootstrap' ? 3 : null
  const other = !focused && usable && !(active === 'local' && setup && names) ? <div role="group" aria-label="其他方式" className="border-t pt-4">
    <div className="-ms-3 flex flex-wrap gap-x-2 text-muted-foreground">
      {active !== 'local' && device.localSetup ? <Button variant="ghost" type="button" onClick={() => chooseLane('local')}><Laptop aria-hidden="true" />{setup ? '设置这台电脑' : computerName}</Button> : null}
      {active !== 'saved' && device.entries.length > 0 && !removed ? <Button variant="ghost" type="button" onClick={() => chooseLane('saved')}><MonitorSmartphone aria-hidden="true" />用过的地址</Button> : null}
      {active !== 'invite' ? <Button variant="ghost" type="button" onClick={() => chooseLane('invite')}><Link2 aria-hidden="true" />用加入链接</Button> : null}
      {active !== 'recovery' ? <Button variant="ghost" type="button" onClick={() => chooseLane('recovery')}><ArchiveRestore aria-hidden="true" />用恢复文件找回</Button> : null}
    </div>
  </div> : null

  let body: ReactNode
  // The join card holds its own failure, next to where a new link goes in.
  let alertInBody = false
  if (device.phase === 'blocked') body = <EmptyState icon={ShieldAlert} title="请用本机地址或 HTTPS 打开" />
  else if (device.phase === 'key_lost') body = <Panel icon={ShieldAlert} title="需要重新加入">
    <p className="text-muted-foreground">这个浏览器保存的登录信息不见了。</p>
    <Button type="button" className="w-fit justify-self-end" disabled={busy} onClick={() => void run(device.replaceLostKey)}>重新开始</Button>
  </Panel>
  else if (device.phase === 'removed') body = <Removed busy={busy} run={run} />
  else if (device.phase === 'restarting') body = <Panel icon={LoaderCircle} busy title="正在重新开始">
    <p className="text-muted-foreground">旧数据已改名保留。这台电脑重启好后，用加入链接把它加回来。</p>
  </Panel>
  else if (device.recovery) body = <RecoveryWaiting />
  else if (device.nodeJoining) body = <NodeJoin />
  else if (device.review?.kind === 'bootstrap') body = <RecoveryStep busy={busy} run={run} />
  else if (device.review) body = <RecoveryReview />
  else if (device.joining) body = <BrowserJoin />
  else if (active === 'local' && setup && !names) body = <NamesStep initial={draftNames} onNext={setNames} />
  else if (active === 'local' && setup && names) body = <PasswordStep busy={busy || linking} disabled={!usable} names={names} onBack={() => { setDraftNames(names); setNames(null) }} run={run} />
  else if (active === 'local' && device.localSetup) body = <Panel icon={Laptop} title={device.localSetup.kind === 'initialized' ? computerName : '这台电脑'}>
    {device.localSetup.kind === 'initialized'
      ? <Button type="button" className="w-fit justify-self-end" disabled={linking || !usable} onClick={() => void run(device.openLocalAccess)}>打开</Button>
      : <p className="text-muted-foreground">用已有设备的加入链接，或恢复文件</p>}
  </Panel>
  else if (active === 'saved') body = <Panel icon={MonitorSmartphone} title="用过的地址">
    <ul className="grid gap-1">{device.entries.map((entry) => {
      const name = device.entryName(entry) || addressName(entry.address), host = hostOf(entry.address)
      return <li key={`${entry.address}:${entry.device_id}:${entry.local === true}`} className="flex min-h-11 items-center rounded-lg bg-muted/60">
        <Button type="button" variant="ghost" className="h-auto min-h-11 min-w-0 flex-1 justify-start" title={entry.address} disabled={linking || !usable} onClick={() => void run(() => device.connect(entry))}>
          <span className="truncate">{name}{host && host !== name ? <span className="ms-2 text-meta text-muted-foreground">{host}</span> : null}</span>
        </Button>
        <Button type="button" variant="ghost" size="icon" className="size-11 shrink-0 text-muted-foreground" aria-label={`移除 ${name}`} title="移除" disabled={linking} onClick={() => {
          try { device.removeEntry(entry); if (device.entries.length === 1) setLane(null) } catch (cause) { setError(messageOf(cause)) }
        }}><X aria-hidden="true" /></Button>
      </li>
    })}</ul>
  </Panel>
  else if (active === 'recovery') body = usable ? <RecoveryControls only="restore" /> : null
  else {
    alertInBody = true
    body = <Panel icon={publicEntry ? QrCode : Link2} title={publicEntry ? '扫码加入' : thisComputer ? '把这台电脑加入我的设备' : removed ? '重新加入' : '加入我的设备'}>
      {/* A failure or the removal notice already says where to go next. */}
      {failed || removed ? null : <p className="-mt-2 text-meta text-muted-foreground">链接来自{publicEntry && !thisComputer ? '你的电脑' : '已有设备'}上的“添加设备”</p>}
      {alert}
      {scanning && purpose !== 'link' ? <Scanner onTicket={(ticket) => { setInvitation(ticket); setScanning(false) }} onCancel={() => setScanning(false)} /> : <form className="grid gap-4" onSubmit={submitInvitation}>
        <fieldset disabled={linking || !usable} className="grid gap-4">
          {purpose !== 'link' && scannable ? <Button type="button" className="w-fit" onClick={() => setScanning(true)}><Camera aria-hidden="true" />扫码</Button> : null}
          {purpose === 'empty' || purpose === 'invalid' ? <>
            <Textarea className="h-20 min-h-0 resize-none field-sizing-fixed font-mono text-meta" name="invitation" required value={invitation} aria-label="加入链接" aria-invalid={purpose === 'invalid' || undefined} autoComplete="off" spellCheck={false} placeholder={scannable ? '或粘贴加入链接' : '粘贴加入链接'} onChange={(event) => setInvitation(event.target.value)} />
            {purpose === 'invalid' ? <InlineError error="这不是有效的加入链接" /> : null}
          </> : <p className="flex min-h-11 items-center gap-2 rounded-lg bg-muted/60 ps-3">
            <CircleCheck aria-hidden="true" className="size-4 shrink-0 text-success" />
            <span className="min-w-0 flex-1 truncate">{invitation.trim().startsWith('vibeface1') ? '已读取二维码' : '已读取加入链接'}</span>
            <Button ref={claimFocus} type="button" variant="ghost" onClick={() => setInvitation('')}>换一个</Button>
          </p>}
          {thisComputer
            ? <Field label="这台电脑叫什么？"><Input name="name" required maxLength={128} autoComplete="off" defaultValue={defaultComputerName()} /></Field>
            : <Field label="这个浏览器叫什么？"><Input name="name" required maxLength={128} autoComplete="off" defaultValue={lastBrowserName() || defaultBrowserName(device.identity?.deviceId)} /></Field>}
          {purpose === 'link' ? <Button type="submit" className="w-fit justify-self-end" aria-busy={linking || undefined}>{linking ? <LoaderCircle aria-hidden="true" className="animate-spin" /> : null}继续</Button> : null}
        </fieldset>
      </form>}
      {publicEntry || thisComputer ? null : <Details label="加入一台电脑"><p className="text-meta text-muted-foreground">在那台电脑上运行 <code>vibeface join</code> 加这条链接。</p></Details>}
    </Panel>
  }

  return <AuthPage header={<>
    {standalone() && !device.entries.length && !focused ? <p className="text-meta text-muted-foreground">主屏幕上的 Vibeface 需要单独加入一次</p> : null}
    {step ? <Steps step={step} /> : null}
  </>}>
    {linking && !focused ? <p role="status" className="flex items-center gap-2 text-meta text-info"><LoaderCircle aria-hidden="true" className="size-4 animate-spin" />{device.phase === 'reconnecting' ? '正在重连' : '连接中'}</p> : null}
    {alertInBody ? null : alert}
    {device.phase === 'initializing' ? <Skeleton className="h-48" /> : body}
    {other}
    {device.identity && !focused ? <Details key={step ?? active}>
      <CopyDiagnostics lines={[['这个浏览器', device.identity.deviceId]]} />
      {usable ? <Details label="手动填写地址">
        <form className="grid gap-3" noValidate onSubmit={(event) => { event.preventDefault(); const form = event.currentTarget; void run(() => device.connect(parseEntry({ address: value(form, 'address'), device_id: value(form, 'device_id') }))) }}>
          <fieldset disabled={linking} className="grid gap-3">
            <Field label="访问地址"><Input type="url" name="address" defaultValue={typeof location === 'undefined' ? '' : location.origin} /></Field>
            <Field label="设备编号"><Input className="font-mono" name="device_id" autoComplete="off" autoCapitalize="none" spellCheck={false} /></Field>
            <Button type="submit" variant="outline" className="w-fit">连接</Button>
          </fieldset>
        </form>
      </Details> : null}
    </Details> : null}
  </AuthPage>
}

type Names = { computer: string; browser: string }

/** The browser's suggested name is usually fine, so only the computer's name asks for a thought. */
function NamesStep({ computer, initial, onNext }: { computer?: string; initial: Names | null; onNext(names: Names): void }) {
  const [browser, setBrowser] = useState(initial?.browser ?? defaultBrowserName())
  const [renaming, setRenaming] = useState(false)
  return <Panel icon={Laptop} title="给这台电脑起名">
    <form className="grid gap-4" onSubmit={(event) => { event.preventDefault(); const form = event.currentTarget; onNext({ computer: value(form, 'computer').trim(), browser: browser.trim() || defaultBrowserName() }) }}>
      <Input key={computer} name="computer" aria-label="这台电脑叫什么？" required maxLength={128} autoComplete="off" defaultValue={initial?.computer || computer || defaultComputerName()} />
      {renaming
        ? <Field label="这个浏览器叫什么？"><Input name="browser" required maxLength={128} autoComplete="off" autoFocus value={browser} onChange={(event) => setBrowser(event.target.value)} /></Field>
        : <p className="flex min-w-0 items-center gap-2 text-meta text-muted-foreground">
          <Monitor aria-hidden="true" className="size-4 shrink-0" /><span className="sr-only">这个浏览器：</span>
          <span className="min-w-0 truncate" title={browser}>{browser}</span>
          <Button type="button" variant="ghost" size="icon" className="-my-2" aria-label="改浏览器名称" title="改浏览器名称" onClick={() => setRenaming(true)}><Pencil aria-hidden="true" /></Button>
        </p>}
      <Button type="submit" className="w-fit justify-self-end">下一步</Button>
    </form>
  </Panel>
}

type Run = (action: () => Promise<void>) => Promise<void>

/** Step 2: the recovery password; the file is sealed with it right away. */
function PasswordStep({ busy, disabled, names, onBack, run }: { busy: boolean; disabled: boolean; names: Names; onBack(): void; run: Run }) {
  const device = useDevice()
  const [short, setShort] = useState(false)
  return <Panel icon={KeyRound} title="设置恢复密码">
    <p className="text-muted-foreground">设备都丢了时，用恢复文件和密码找回。</p>
    <form className="grid gap-4" noValidate onSubmit={(event) => {
      event.preventDefault()
      const form = event.currentTarget
      const input = form.elements.namedItem('password') as HTMLInputElement
      if ([...input.value].length < 8) { setShort(true); input.focus(); return }
      void run(async () => {
        try { await device.createLocalNetwork(names.browser, passphrase(value(form, 'password')), names.computer) }
        finally { form.reset() }
      })
    }}>
      <fieldset disabled={busy || disabled} className="grid gap-4">
        <Field label="恢复密码">
          <PasswordInput name="password" autoComplete="new-password" aria-invalid={short || undefined} aria-describedby="password-hint" onChange={() => setShort(false)} />
          <span id="password-hint" className={cn('font-normal', short ? 'text-danger' : 'text-muted-foreground')}>至少 8 个字符，忘了就无法找回</span>
        </Field>
        <div className="flex justify-between gap-2">
          <Button type="button" variant="ghost" className="-ms-3" onClick={onBack}>上一步</Button>
          <Button type="submit" aria-busy={busy || undefined}>{busy ? <LoaderCircle aria-hidden="true" className="animate-spin" /> : null}下一步</Button>
        </div>
      </fieldset>
    </form>
  </Panel>
}

/** Step 3: download the sealed file, confirm it is stored apart from the password, then create. */
function RecoveryStep({ busy, run }: { busy: boolean; run: Run }) {
  const device = useDevice()
  const [downloaded, setDownloaded] = useState(false)
  const [saved, setSaved] = useState(false)
  const [code, setCode] = useState('')
  const networkId = device.review?.networkId
  useEffect(() => {
    if (!networkId) return
    let live = true
    void recoveryCode(networkId).then((value) => { if (live) setCode(value) })
    return () => { live = false }
  }, [networkId])
  return <Panel icon={FileKey} title="保存恢复文件">
    <SaveRecoveryFile code={code} saved={saved} onSaved={setSaved} busy={busy} download={{ done: downloaded, run: () => void run(async () => { await device.downloadBackup(); setDownloaded(true) }) }} />
    <div className="flex justify-between gap-2">
      <Button type="button" variant="ghost" className="-ms-3" disabled={busy} onClick={device.cancelSensitive}>上一步</Button>
      <Button type="button" disabled={!saved || busy} aria-busy={busy || undefined} onClick={() => void run(async () => { startOnboarding(); await device.confirmBootstrap(true) })}>{busy ? <LoaderCircle aria-hidden="true" className="animate-spin" /> : null}完成创建</Button>
    </div>
  </Panel>
}

/** A browser joining through a link: it shows the safety code and waits for an admin to allow it. */
function BrowserJoin() {
  const device = useDevice()
  const joining = device.joining!
  return <Panel icon={Hourglass} title="等待批准">
    {device.info ? <p className="flex items-center gap-2 text-meta text-muted-foreground"><ShieldCheck aria-hidden="true" className="size-4 shrink-0 text-success" />已连上你的设备</p> : null}
    <JoinCode code={joining.code} />
    {device.phase === 'reconnecting' ? <p role="status" className="flex items-center gap-2 text-meta text-info"><LoaderCircle aria-hidden="true" className="size-4 animate-spin" />正在重连，请求还在</p> : null}
    <Button type="button" variant="ghost" className="-ms-3 w-fit" onClick={device.cancelJoin}>取消</Button>
    <Details><CopyDiagnostics lines={[['这个浏览器', device.identity?.deviceId], ['加入请求', joining.join_id]]} /></Details>
  </Panel>
}
