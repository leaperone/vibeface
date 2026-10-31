import { useEffect, useRef, useState, type FormEvent, type ReactNode } from 'react'
import { Archive, ArchiveRestore, Camera, CircleCheck, CirclePlus, FileKey, Hourglass, KeyRound, Laptop, LifeBuoy, Link2, LoaderCircle, LogIn, MessageSquareLock, MonitorSmartphone, MonitorX, QrCode, RotateCcw, Settings2, ShieldAlert, ShieldCheck, X } from 'lucide-react'
import { AuthPage, Details, EmptyState, InlineError, Panel, PasswordInput, claimFocus, SafetyCode, Skeleton, Steps, WaitingDot } from '@/components/app/kit'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Textarea } from '@/components/ui/textarea'
import { addressName, hostOf } from '@vibeface/client/workspace/labels.ts'
import { entryAddress, entryKey, messageOf, parseEntry } from '@vibeface/client/lib/node.ts'
import { spentInvite, spentInviteText } from '@vibeface/client/lib/errors.ts'
import { passphrase, passwordStrength, recoveryCode } from '@vibeface/client/lib/secure.ts'
import { defaultBrowserName, defaultComputerName, lastBrowserName, rememberBrowserName } from '@/lib/device-name'
import { startOnboarding } from './onboarding-flag'
import { primaryLane } from '@vibeface/client/identity/connection-lane.ts'
import { notJoined, removedText, useDevice } from '@vibeface/client/identity/context.tsx'
import { joinCopy, manualCopy, recoveryCopy, startCopy } from '@vibeface/client/identity/copy.ts'
import { localCopy } from '@/features/workspace/local-session'
import { webPlatform } from '@/lib/platform'

const NOT_JOINED = notJoined(webPlatform.self)
const BROWSER_REMOVED = removedText(webPlatform.self)
import { PasswordStrength, RecoveryControls, RecoveryReview, RecoveryWaiting, SaveRecoveryFile } from './recovery-controls'
import { CopyDiagnostics } from './device-controls'
import { joinTicket } from '@vibeface/client/lib/transport.ts'

type Lane = 'local' | 'invite' | 'saved' | 'recovery'
type Purpose = 'empty' | 'link' | 'invalid'
const value = (form: HTMLFormElement, key: string) => { const field = form.elements.namedItem(key); return field instanceof HTMLInputElement || field instanceof HTMLTextAreaElement ? field.value : '' }

/** `decodes`: whether the ticket in it reads as one, checked as it is pasted, as on the phone. */
function purposeOf(invitation: string, decodes: (link: string) => boolean): Purpose {
  if (!invitation.trim()) return 'empty'
  return joinTicket(invitation) && decodes(invitation) ? 'link' : 'invalid'
}

type Detector = { detect(source: HTMLVideoElement): Promise<Array<{ rawValue: string }>> }
const detectorClass = () => (globalThis as { BarcodeDetector?: new (options: { formats: string[] }) => Detector }).BarcodeDetector
const canScan = () => typeof navigator !== 'undefined' && Boolean(navigator.mediaDevices?.getUserMedia) && Boolean(detectorClass())
/** A phone or tablet, whose camera faces the code; on a desktop pasting leads and scanning steps aside. */
const touchFirst = () => typeof matchMedia === 'function' && matchMedia('(pointer: coarse)').matches

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
  const commands = <Details><a href="https://github.com/leaperone/vibeface/blob/main/docs/deployment.md" target="_blank" rel="noreferrer" className="underline">用命令行重新加入</a></Details>
  // On its own local page the rejoin first opens this computer, then carries on there.
  const setup = device.localSetup?.kind === 'initialized' && device.localSetup.removed ? device.localSetup : null
  const here = Boolean(setup)
  const canRejoin = device.removedLocally || Boolean(setup?.local_launch)
  return <Panel icon={MonitorX} title={here || device.removedLocally ? '这台电脑已被移出' : '所连的电脑已被移出'}>
    {canRejoin ? <>
      <p className="text-muted-foreground">点重新加入，再用已有设备的加入链接把它加回来</p>
      <p className="flex items-center gap-2 text-meta text-muted-foreground"><Archive aria-hidden="true" className="size-4 shrink-0" />旧数据改名保留，不会删除</p>
      <Button type="button" className="w-fit justify-self-end" disabled={busy} onClick={() => void run(device.rejoin)}>{busy ? <LoaderCircle aria-hidden="true" className="animate-spin" /> : <RotateCcw aria-hidden="true" />}重新加入</Button>
      {commands}
    </> : <>
      {here ? null : <p className="text-muted-foreground">在它的本机页面点“重新加入”。</p>}
      {commands}
    </>}
  </Panel>
}

/** The code appears once the device that sent the link shows its own, so until then the next step is over there. */
function JoinCode({ code }: { code?: string }) {
  if (!code) return <p role="status" className="flex items-center gap-2 text-muted-foreground"><LoaderCircle aria-hidden="true" className="size-4 shrink-0 animate-spin" />{joinCopy.showCode}</p>
  return <>
    <p role="status" className="flex items-center gap-2 text-muted-foreground"><WaitingDot />{joinCopy.compareCode}</p>
    <SafetyCode code={code} />
  </>
}

/** The Node's own join: it shows the short code the manager compares before approving. */
function NodeJoin() {
  const device = useDevice()
  const joining = device.nodeJoining!
  const [cancelling, setCancelling] = useState(false)
  return <Panel icon={Hourglass} title={joining.status === 'joined' ? '已加入' : joinCopy.waiting}>
    {joining.status === 'joined' ? <p className="flex items-center gap-2 text-success"><CircleCheck aria-hidden="true" className="size-4 shrink-0" />正在打开这台电脑</p> : <>
      {joining.code ? <Reached /> : null}
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

/**
 * The computer that served this page. A public entry only forwards to people's own devices, so its page only opens join links.
 * Its `deviceId` fills in a manual address that is this page's own, which the page already trusts by running its code.
 */
function usePageNode() {
  const [node, setNode] = useState<{ publicEntry: boolean; deviceId: string }>({ publicEntry: false, deviceId: '' })
  useEffect(() => {
    let live = true
    if (typeof fetch === 'function') fetch('/api/info', { cache: 'no-store', credentials: 'omit' })
      .then(response => response.ok ? response.json() : null)
      .then(info => { if (live) setNode({ publicEntry: info?.public_entry === true, deviceId: typeof info?.device_id === 'string' ? info.device_id : '' }) })
      .catch(() => {})
    return () => { live = false }
  }, [])
  return node
}

const sameOrigin = (address: string) => { try { return new URL(address).origin === location.origin } catch { return false } }

/** Opened from the Home Screen: iOS keeps such an app's storage apart from Safari's. */
const standalone = () => typeof window !== 'undefined' && (window.matchMedia?.('(display-mode: standalone)').matches || (navigator as { standalone?: boolean }).standalone === true)

/** `lane`: the way in to show first, as the join page does. */
export function ConnectionScreen({ lane: firstLane }: { lane?: Lane } = {}) {
  const device = useDevice()
  const pageNode = usePageNode()
  const publicEntry = pageNode.publicEntry
  const generation = device.connectionGeneration()
  const [busyGeneration, setBusyGeneration] = useState<number | null>(null)
  const busy = busyGeneration === generation
  const pending = useRef<number | null>(null)
  const [error, setError] = useState('')
  const [notice, setNotice] = useState('')
  const [invitation, setInvitation] = useState(device.initialInvitation)
  const [lane, setLane] = useState<Lane | null>(firstLane ?? null)
  const [names, setNames] = useState<Names | null>(null)
  // A computer not set up yet first asks whether it starts a network or joins one.
  const [creating, setCreating] = useState(false)
  // Going back from the password keeps what was typed.
  const [draftNames, setDraftNames] = useState<Names | null>(null)
  const [scanning, setScanning] = useState(false)
  const scannable = canScan()
  const scanFirst = scannable && touchFirst()
  const savedName = device.localSetup?.kind === 'initialized' ? device.entryName(device.localSetup.entry) : ''
  const computerName = savedName || '这台电脑'
  const removed = device.error === BROWSER_REMOVED
  // A computer in use opens fully only for a browser already in; any other browser joins, or just manages it from here.
  const localSetup = device.localSetup
  const memberHere = localSetup?.kind === 'initialized' && device.entries.some((item) => entryKey(item) === entryKey(localSetup.entry))
  const local = !localSetup ? 'none' : localSetup.kind === 'uninitialized' || memberHere ? 'open' : 'join'
  const suggested = primaryLane({ hasInvitation: Boolean(device.initialInvitation), savedCount: device.entries.length, local, rejoined: device.rejoined, removed })
  const active = lane ?? suggested
  // The ticket decoder loads with this browser's key; until then only the link's shape is checked.
  const purpose = purposeOf(invitation, (link) => !device.identity || device.linkTarget(link) !== null)
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

  // A used or expired link says so as it opens, before the name and 加入; no answer leaves the form as it is.
  const checkable = purpose === 'link' && usable && !device.joining && !device.nodeJoining
  useEffect(() => {
    if (!checkable) return
    let live = true
    void device.checkInvite(invitation).then((state) => {
      // A join that just ended spent the link too; its own reason (say, 拒绝) stays, not 已用过.
      if (live && (state === 'used' || state === 'expired')) { setInvitation(''); setError((current) => current || spentInviteText) }
    })
    return () => { live = false }
  }, [invitation, checkable, thisComputer])

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
  const choosing = active === 'local' && setup && !names && !creating
  const step = active === 'local' && setup && !focused ? (names ? 2 : creating ? 1 : null) : device.review?.kind === 'bootstrap' ? 3 : null
  const other = !focused && usable && !(active === 'local' && setup && (names || creating)) ? <div role="group" aria-label="其他方式" className="border-t pt-4">
    <div className="-ms-3 flex flex-wrap gap-x-2 text-muted-foreground">
      {active !== 'local' && device.localSetup ? <Button variant="ghost" type="button" onClick={() => { if (setup) setCreating(true); chooseLane('local') }}>{setup ? <CirclePlus aria-hidden="true" /> : <Laptop aria-hidden="true" />}{setup ? startCopy.create : local === 'open' ? computerName : localCopy.manage}</Button> : null}
      {active !== 'saved' && device.entries.length > 0 && !removed ? <Button variant="ghost" type="button" onClick={() => chooseLane('saved')}><MonitorSmartphone aria-hidden="true" />用过的地址</Button> : null}
      {active !== 'invite' && !choosing ? <Button variant="ghost" type="button" onClick={() => chooseLane('invite')}><Link2 aria-hidden="true" />{joinCopy.byLink}</Button> : null}
      {active !== 'recovery' ? <Button variant="ghost" type="button" onClick={() => chooseLane('recovery')}><ArchiveRestore aria-hidden="true" />用恢复文件找回<span className="text-meta font-normal">{recoveryCopy.whenLost}</span></Button> : null}
    </div>
  </div> : null

  let body: ReactNode
  // The join card holds its own failure, next to where a new link goes in.
  let alertInBody = false
  if (device.phase === 'blocked') body = <EmptyState icon={ShieldAlert} title="请在这台电脑上打开，或用加入链接" />
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
  else if (choosing) body = <StartChoice onCreate={() => setCreating(true)} onJoin={() => chooseLane('invite')} />
  else if (active === 'local' && setup && !names) body = <NamesStep initial={draftNames} onNext={setNames} onBack={() => setCreating(false)} />
  else if (active === 'local' && setup && names) body = <PasswordStep busy={busy || linking} disabled={!usable} names={names} onBack={() => { setDraftNames(names); setNames(null) }} run={run} />
  else if (active === 'local' && device.localSetup) body = <Panel icon={Laptop} title={device.localSetup.kind === 'initialized' ? computerName : '这台电脑'}>
    {device.localSetup.kind !== 'initialized' ? <p className="text-muted-foreground">用已有设备的加入链接，或恢复文件</p>
      : local === 'open' ? <Button type="button" className="w-full sm:w-fit sm:justify-self-end" disabled={linking || !usable} onClick={() => void run(device.openLocalAccess)}><LogIn aria-hidden="true" />打开</Button>
      : <>
        <p className="flex items-center gap-2 text-muted-foreground"><MessageSquareLock aria-hidden="true" className="size-4 shrink-0" />{localCopy.note}</p>
        {device.localSetup.local_launch ? <Button type="button" variant="outline" className="w-full sm:w-fit sm:justify-self-end" disabled={linking || !usable} onClick={() => void run(device.openLocalAccess)}><Settings2 aria-hidden="true" />{localCopy.manage}</Button> : null}
      </>}
  </Panel>
  else if (active === 'saved') body = <Panel icon={MonitorSmartphone} title="用过的地址">
    <ul className="grid gap-1">{device.entries.map((entry) => {
      const name = device.entryName(entry) || addressName(entryAddress(entry)), host = hostOf(entryAddress(entry))
      return <li key={entryKey(entry)} className="flex min-h-11 items-center rounded-lg bg-muted/60">
        <Button type="button" variant="ghost" className="h-auto min-h-11 min-w-0 flex-1 justify-start" title={entryAddress(entry)} disabled={linking || !usable} onClick={() => void run(() => device.connect(entry))}>
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
      {failed || removed ? null : <p className="-mt-2 text-meta text-muted-foreground">{joinCopy.linkSource(publicEntry && !thisComputer ? '你的电脑' : undefined)}</p>}
      {alert}
      {scanning && purpose !== 'link' ? <Scanner onTicket={(ticket) => { setInvitation(ticket); setScanning(false) }} onCancel={() => setScanning(false)} /> : <form className="grid gap-4" onSubmit={submitInvitation}>
        <fieldset disabled={linking || !usable} className="grid gap-4">
          {purpose !== 'link' && scanFirst ? <Button type="button" className="w-fit" onClick={() => setScanning(true)}><Camera aria-hidden="true" />{joinCopy.scan}</Button> : null}
          {purpose === 'empty' || purpose === 'invalid' ? <>
            <Textarea ref={scanFirst ? undefined : claimFocus} className="h-20 min-h-0 resize-none field-sizing-fixed font-mono text-meta" name="invitation" required value={invitation} aria-label="加入链接" aria-invalid={purpose === 'invalid' || undefined} autoComplete="off" spellCheck={false} placeholder={scanFirst ? joinCopy.paste : joinCopy.pasteFirst} onChange={(event) => setInvitation(event.target.value)} />
            {purpose === 'invalid' ? <InlineError error={joinCopy.notLink} /> : null}
            {scannable && !scanFirst ? <Button type="button" variant="ghost" className="-ms-3 w-fit text-muted-foreground" onClick={() => setScanning(true)}><Camera aria-hidden="true" />{joinCopy.scan}</Button> : null}
          </> : <p className="flex min-h-11 items-center gap-2 rounded-lg bg-muted/60 ps-3">
            <Link2 aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" />
            <span className="min-w-0 flex-1 truncate">{joinCopy.linkFilled}</span>
            <Button ref={claimFocus} type="button" variant="ghost" onClick={() => setInvitation('')}>换一个</Button>
          </p>}
          {thisComputer
            ? <Field label="这台电脑叫什么？"><Input name="name" required maxLength={128} autoComplete="off" defaultValue={defaultComputerName()} /></Field>
            : <Field label="这个浏览器叫什么？"><Input name="name" required maxLength={128} autoComplete="off" defaultValue={lastBrowserName() || defaultBrowserName()} /></Field>}
          {purpose === 'link' ? <Button type="submit" className="w-fit justify-self-end" aria-busy={linking || undefined}>{linking ? <LoaderCircle aria-hidden="true" className="animate-spin" /> : null}{linking ? joinCopy.joining : joinCopy.join}</Button> : null}
        </fieldset>
      </form>}
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
    {device.identity && !focused && step !== 2 ? <Details key={step ?? active}>
      {alertInBody && !publicEntry && !thisComputer ? <p>另一台电脑上运行 <code>vibeface join</code> 加这条链接。</p> : null}
      <CopyDiagnostics lines={[['这个浏览器', device.identity.deviceId]]} />
      {usable ? <Details label="手动填写地址">
        <form className="grid gap-3" noValidate onSubmit={(event) => {
          event.preventDefault()
          const form = event.currentTarget, address = value(form, 'address')
          const deviceId = value(form, 'device_id').trim() || (!publicEntry && sameOrigin(address) ? pageNode.deviceId : '')
          void run(() => device.connect(parseEntry({ address, device_id: deviceId })))
        }}>
          <fieldset disabled={linking} className="grid gap-3">
            <Field label="访问地址"><Input type="url" name="address" defaultValue={typeof location === 'undefined' ? '' : location.origin} /></Field>
            <Field label="设备编号"><Input className="font-mono" name="device_id" placeholder={publicEntry ? undefined : manualCopy.optional} autoComplete="off" autoCapitalize="none" spellCheck={false} /></Field>
            <p className="-mt-1 text-muted-foreground">{manualCopy.where}</p>
            <Button type="submit" variant="outline" className="w-fit">连接</Button>
          </fieldset>
        </form>
      </Details> : null}
    </Details> : null}
  </AuthPage>
}

type Names = { computer: string; browser: string }

/** Two equal ways on for a computer not set up yet: a second computer joins the first instead of starting another network. */
function StartChoice({ onCreate, onJoin }: { onCreate(): void; onJoin(): void }) {
  const option = (id: string, Icon: typeof Link2, title: string, note: string, onClick: () => void) => <Button type="button" variant="outline" aria-label={title} aria-describedby={id} className="h-auto min-h-24 flex-col items-start justify-start gap-3 whitespace-normal p-4 text-start" onClick={onClick}>
    <span className="grid size-9 shrink-0 place-items-center rounded-xl bg-muted text-muted-foreground ring-1 ring-border"><Icon aria-hidden="true" className="size-5" /></span>
    <span className="grid gap-0.5"><span className="text-heading font-semibold">{title}</span><span id={id} className="text-meta font-normal text-muted-foreground">{note}</span></span>
  </Button>
  return <div role="group" aria-label="开始使用" className="grid gap-3 sm:grid-cols-2">
    {option('start-create-note', CirclePlus, startCopy.create, startCopy.createNote, onCreate)}
    {option('start-join-note', Link2, startCopy.join, startCopy.joinNote, onJoin)}
  </div>
}

/** Only the computer's name asks for a thought; the browser takes its suggested name unless someone opens it. */
function NamesStep({ computer, initial, onNext, onBack }: { computer?: string; initial: Names | null; onNext(names: Names): void; onBack(): void }) {
  return <Panel icon={Laptop} title="给这台电脑起名">
    <form className="grid gap-4" onSubmit={(event) => { event.preventDefault(); const form = event.currentTarget; onNext({ computer: value(form, 'computer').trim(), browser: value(form, 'browser').trim() || defaultBrowserName() }) }}>
      <Input key={computer} name="computer" aria-label="这台电脑叫什么？" required maxLength={128} autoComplete="off" defaultValue={initial?.computer || computer || defaultComputerName()} />
      <Details label="这个浏览器的名字">
        <Input name="browser" aria-label="这个浏览器叫什么？" maxLength={128} autoComplete="off" className="text-foreground" defaultValue={initial?.browser ?? defaultBrowserName()} />
      </Details>
      <div className="flex justify-between gap-2">
        <Button type="button" variant="ghost" className="-ms-3" onClick={onBack}>上一步</Button>
        <Button type="submit">下一步</Button>
      </div>
    </form>
  </Panel>
}

type Run = (action: () => Promise<void>) => Promise<void>

/** Step 2: the recovery password; the file is sealed with it right away. */
function PasswordStep({ busy, disabled, names, onBack, run }: { busy: boolean; disabled: boolean; names: Names; onBack(): void; run: Run }) {
  const device = useDevice()
  const [password, setPassword] = useState('')
  const [tried, setTried] = useState(false)
  const weak = passwordStrength(password).level === 'weak'
  return <Panel icon={KeyRound} title="设置恢复密码">
    <p className="-mt-2 flex items-center gap-2 text-meta text-muted-foreground"><LifeBuoy aria-hidden="true" className="size-4 shrink-0" />{recoveryCopy.passwordWhy}</p>
    <form className="grid gap-4" noValidate onSubmit={(event) => {
      event.preventDefault()
      const form = event.currentTarget
      const input = form.elements.namedItem('password') as HTMLInputElement
      if (passwordStrength(input.value).level === 'weak') { setTried(true); input.focus(); return }
      void run(async () => {
        try { await device.createLocalNetwork(names.browser, passphrase(value(form, 'password')), names.computer) }
        finally { form.reset(); setPassword('') }
      })
    }}>
      <fieldset disabled={busy || disabled} className="grid gap-4">
        <div className="grid gap-1.5">
          <Field label="恢复密码">
            <PasswordInput name="password" autoComplete="new-password" aria-invalid={tried && weak || undefined} aria-describedby="password-hint" onChange={(event) => { setPassword(event.target.value); setTried(false) }} />
          </Field>
          <PasswordStrength id="password-hint" password={password} tried={tried} />
        </div>
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
    {device.info ? <Reached /> : null}
    <JoinCode code={joining.code} />
    {device.phase === 'reconnecting' ? <p role="status" className="flex items-center gap-2 text-meta text-info"><LoaderCircle aria-hidden="true" className="size-4 animate-spin" />正在重连，请求还在</p> : null}
    <Button type="button" variant="ghost" className="-ms-3 w-fit" onClick={device.cancelJoin}>取消</Button>
    <Details><CopyDiagnostics lines={[['这个浏览器', device.identity?.deviceId], ['加入请求', joining.join_id]]} /></Details>
  </Panel>
}

/** The request reached one of the person's devices; both join pages say it the same way. */
function Reached() {
  return <p className="flex items-center gap-2 text-meta text-muted-foreground"><ShieldCheck aria-hidden="true" className="size-4 shrink-0 text-success" />{joinCopy.reached}</p>
}
