import { useEffect, useRef, useState, type FormEvent, type ReactNode } from 'react'
import { ArchiveRestore, Camera, CircleCheck, Eye, EyeOff, FileKey, Hourglass, Laptop, Link2, LoaderCircle, MonitorSmartphone, MonitorX, QrCode, RotateCcw, ShieldAlert, ShieldCheck, type LucideIcon } from 'lucide-react'
import { cn } from 'cn'
import { Details, DetailRow, EmptyState, InlineError, SafetyCode, Skeleton } from '@/components/app/kit'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Textarea } from '@/components/ui/textarea'
import { addressName } from '@vibeface/client/workspace/labels.ts'
import { messageOf, parseEntry } from '@vibeface/client/lib/node.ts'
import { passphrase } from '@vibeface/client/lib/secure.ts'
import { defaultBrowserName, defaultComputerName } from '@/lib/device-name'
import { startOnboarding } from './onboarding-flag'
import { primaryLane } from '@vibeface/client/identity/connection-lane.ts'
import { notJoined, useDevice } from '@vibeface/client/identity/context.tsx'
import { webPlatform } from '@/lib/platform'

const NOT_JOINED = notJoined(webPlatform.self)
import { RecoveryControls, RecoveryReview } from './recovery-controls'
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
    }).catch(() => { if (live) setError('没能打开相机。请允许使用相机，或把链接粘贴到这里。') })
    return () => { live = false; clearTimeout(timer); stream?.getTracks().forEach(track => track.stop()) }
  }, [])
  return <div className="grid gap-3">
    {error ? <p role="alert" className="text-meta text-danger">{error}</p> : <video ref={video} className="aspect-square w-full rounded-lg bg-black object-cover" playsInline muted aria-label="对准二维码" />}
    <Button type="button" variant="ghost" className="-ms-3 w-fit" onClick={onCancel}>不扫了，粘贴链接</Button>
  </div>
}


function Steps({ step }: { step: number }) {
  return <div className="flex items-center gap-3 text-meta text-muted-foreground">
    <span className="flex gap-1.5" aria-hidden="true">{[1, 2, 3, 4].map((item) => <span key={item} className={cn('size-2 rounded-full', item <= step ? 'bg-foreground' : 'bg-muted-foreground/30')} />)}</span>
    <span className="sr-only">第 {step} 步，共 4 步</span>
  </div>
}

function Panel({ icon: Icon, title, children }: { icon: LucideIcon; title: string; children: ReactNode }) {
  return <section className="grid gap-4 rounded-xl border bg-card p-5">
    <h2 className="flex items-center gap-2 text-heading font-semibold"><Icon aria-hidden="true" className="size-5 text-muted-foreground" />{title}</h2>
    {children}
  </section>
}

const REJOIN = `vibeface service stop
mv ~/.vibeface ~/.vibeface-removed-$(date +%Y%m%d-%H%M%S)
vibeface init
vibeface join '加入链接'
vibeface service start`

function Removed({ busy, run }: { busy: boolean; run: Run }) {
  const device = useDevice()
  const [copied, setCopied] = useState(false)
  const commands = <>
    <pre className="overflow-x-auto rounded-lg bg-muted/60 px-3 py-2 font-mono text-meta text-foreground">{REJOIN}</pre>
    <Button type="button" variant="outline" className="w-fit" onClick={() => { void navigator.clipboard?.writeText(REJOIN).then(() => setCopied(true)) }}>
      {copied ? <CircleCheck aria-hidden="true" /> : null}{copied ? '已复制' : '复制命令'}
    </Button>
  </>
  return <Panel icon={MonitorX} title="这台设备已被移出">
    {device.removedLocally ? <>
      <p className="text-muted-foreground">旧数据会改名保留，不会删除。</p>
      <Button type="button" className="w-fit" disabled={busy} onClick={() => void run(device.rejoin)}>{busy ? <LoaderCircle aria-hidden="true" className="animate-spin" /> : <RotateCcw aria-hidden="true" />}重新加入</Button>
      <Details label="用命令重新加入">{commands}</Details>
    </> : <>
      <p className="text-muted-foreground">在它的本机页面点“重新加入”。</p>
      <Details label="用命令重新加入">{commands}</Details>
    </>}
  </Panel>
}

/** The Node's own join: it shows the short code the manager compares before approving. */
function NodeJoin() {
  const device = useDevice()
  const joining = device.nodeJoining!
  const [cancelling, setCancelling] = useState(false)
  return <Panel icon={Hourglass} title={joining.status === 'joined' ? '已加入' : '等待批准'}>
    {joining.status === 'joined' ? <p className="flex items-center gap-2 text-success"><CircleCheck aria-hidden="true" className="size-4 shrink-0" />正在打开这台电脑</p> : <>
      <p className="text-muted-foreground">在发链接的设备上比对安全码</p>
      {joining.code ? <SafetyCode code={joining.code} /> : <Skeleton className="h-8 w-40" />}
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
  const [names, setNames] = useState<{ computer: string; browser: string } | null>(null)
  const [scanning, setScanning] = useState(false)
  const scannable = canScan()
  // A removed computer that started over keeps its old name, and joining again is its next step.
  const formerName = device.localSetup?.kind === 'uninitialized' ? device.localSetup.name : undefined
  const suggested = primaryLane({ hasInvitation: Boolean(device.initialInvitation), savedCount: device.entries.length, hasLocalSetup: Boolean(device.localSetup), rejoined: device.rejoined || Boolean(formerName) })
  const active = lane ?? suggested
  const purpose = purposeOf(invitation)
  const usable = !!device.identity && !['blocked', 'key_lost', 'initializing', 'removed', 'restarting'].includes(device.phase)
  const thisComputer = device.localSetup?.kind === 'uninitialized'
  const linking = busy || ['connecting', 'reconnecting'].includes(device.phase)
  useEffect(() => { if (device.initialInvitation) setInvitation((current) => current.trim() ? current : device.initialInvitation) }, [device.initialInvitation])
  useEffect(() => { setError('') }, [generation])

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
      if (!thisComputer) return device.join(invitation, value(form, 'name'))
      // The entry spent the invite on the cancelled request, so it cannot be used again.
      if (await device.joinThisComputer(invitation, value(form, 'name')) === 'cancelled') { setInvitation(''); setNotice('已取消。这条链接已用过，要加入请在已有设备上重新添加。') }
    })
  }

  const deviceError = device.error === NOT_JOINED ? '' : device.error
  const quiet = notice || (!error && device.error === NOT_JOINED ? NOT_JOINED : '')
  const alert = error || deviceError && device.phase !== 'reconnecting' ? <InlineError error={error || deviceError} />
    : quiet ? <p role="status" className="flex items-center gap-2 text-meta text-muted-foreground">{quiet === NOT_JOINED ? <MonitorX aria-hidden="true" className="size-4 shrink-0" /> : null}{quiet}</p> : null
  const focused = Boolean(device.review || device.joining || device.nodeJoining)
  const setup = device.localSetup?.kind === 'uninitialized'
  const step = active === 'local' && setup && !focused ? (names ? 2 : 1) : device.review?.kind === 'bootstrap' ? 2 : null
  const other = !focused && usable && !(active === 'local' && setup && names) ? <div role="group" aria-label="其他方式" className="border-t pt-4">
    <div className="-ms-3 flex flex-wrap gap-2">
      {active !== 'local' && device.localSetup ? <Button variant="ghost" type="button" onClick={() => chooseLane('local')}><Laptop aria-hidden="true" />{setup ? '设置这台电脑' : '这台电脑'}</Button> : null}
      {active !== 'saved' && device.entries.length > 0 ? <Button variant="ghost" type="button" onClick={() => chooseLane('saved')}><MonitorSmartphone aria-hidden="true" />用过的地址</Button> : null}
      {active !== 'invite' ? <Button variant="ghost" type="button" onClick={() => chooseLane('invite')}><Link2 aria-hidden="true" />用加入链接</Button> : null}
      {active !== 'recovery' ? <Button variant="ghost" type="button" onClick={() => chooseLane('recovery')}><ArchiveRestore aria-hidden="true" />用恢复文件找回</Button> : null}
    </div>
  </div> : null

  let body: ReactNode
  if (device.phase === 'blocked') body = <EmptyState icon={ShieldAlert} title="请用本机地址或 HTTPS 打开" />
  else if (device.phase === 'key_lost') body = <Panel icon={ShieldAlert} title="需要重新加入">
    <p className="text-muted-foreground">这个浏览器保存的登录信息不见了。</p>
    <Button type="button" className="w-fit" disabled={busy} onClick={() => void run(device.replaceLostKey)}>重新开始</Button>
  </Panel>
  else if (device.phase === 'removed') body = <Removed busy={busy} run={run} />
  else if (device.phase === 'restarting') body = <Panel icon={LoaderCircle} title="正在重新开始">
    <p className="text-muted-foreground">旧数据已改名保留。这台电脑重启好后，用加入链接把它加回来。</p>
  </Panel>
  else if (device.nodeJoining) body = <NodeJoin />
  else if (device.review?.kind === 'bootstrap') body = <RecoveryStep busy={busy} run={run} />
  else if (device.review) body = <RecoveryReview />
  else if (device.joining) body = <BrowserJoin />
  else if (active === 'local' && setup && !names) body = <NamesStep onNext={setNames} />
  else if (active === 'local' && setup && names) body = <PasswordStep busy={busy || linking} disabled={!usable} names={names} onBack={() => setNames(null)} run={run} />
  else if (active === 'local' && device.localSetup) body = <Panel icon={Laptop} title="这台电脑">
    {device.localSetup.kind === 'initialized'
      ? <Button type="button" className="w-fit" disabled={linking || !usable} onClick={() => void run(device.openLocalAccess)}>打开这台电脑</Button>
      : <p className="text-muted-foreground">用已有设备的加入链接，或恢复文件</p>}
  </Panel>
  else if (active === 'saved') body = <Panel icon={MonitorSmartphone} title="用过的地址">
    <ul className="grid gap-2">{device.entries.map((entry) => <li key={`${entry.address}:${entry.device_id}:${entry.local === true}`} className="flex items-center gap-3 rounded-lg bg-muted/60 px-3 py-2">
      <span className="min-w-0 flex-1 truncate" title={entry.address}>{addressName(entry.address)}</span>
      <Button type="button" variant="outline" disabled={linking || !usable} onClick={() => void run(() => device.connect(entry))}>连接</Button>
    </li>)}</ul>
  </Panel>
  else if (active === 'recovery') body = usable ? <RecoveryControls only="restore" /> : null
  else body = <Panel icon={publicEntry ? QrCode : Link2} title={publicEntry ? '扫码加入' : thisComputer ? '把这台电脑加入我的设备' : '加入我的设备'}>
    {scanning && purpose !== 'link' ? <Scanner onTicket={(ticket) => { setInvitation(ticket); setScanning(false) }} onCancel={() => setScanning(false)} /> : <form className="grid gap-4" onSubmit={submitInvitation}>
      <fieldset disabled={linking || !usable} className="grid gap-4">
        {purpose !== 'link' && scannable ? <Button type="button" className="w-fit" onClick={() => setScanning(true)}><Camera aria-hidden="true" />扫码</Button> : null}
        {purpose === 'empty' || purpose === 'invalid' ? <>
          <Textarea className="h-20 min-h-0 resize-none field-sizing-fixed font-mono text-meta" name="invitation" required value={invitation} aria-label="加入链接" aria-invalid={purpose === 'invalid' || undefined} autoComplete="off" spellCheck={false} placeholder={scannable ? '或粘贴加入链接' : '粘贴加入链接'} onChange={(event) => setInvitation(event.target.value)} />
          {purpose === 'invalid' ? <p role="alert" className="text-meta text-danger">这不是有效的加入链接</p> : null}
        </> : <p className="flex min-h-11 items-center gap-2 rounded-lg bg-muted/60 ps-3">
          <CircleCheck aria-hidden="true" className="size-4 shrink-0 text-success" />
          <span className="min-w-0 flex-1 truncate">{invitation.trim().startsWith('vibeface1') ? '已读取二维码' : '已读取加入链接'}</span>
          <Button type="button" variant="ghost" onClick={() => setInvitation('')}>换一个</Button>
        </p>}
        {thisComputer
          ? <Field label="这台电脑叫什么？"><Input name="name" required maxLength={128} autoComplete="off" defaultValue={formerName || defaultComputerName()} /></Field>
          : <Field label="这个浏览器叫什么？"><Input name="name" required maxLength={128} autoComplete="off" defaultValue={defaultBrowserName(device.identity?.deviceId)} /></Field>}
        {purpose === 'link' ? <Button type="submit" className="w-fit">{linking ? <LoaderCircle aria-hidden="true" className="animate-spin" /> : null}继续</Button> : null}
      </fieldset>
    </form>}
    {publicEntry || thisComputer
      ? <p className="text-meta text-muted-foreground">链接来自{thisComputer ? '已有设备' : '你的电脑'}上的“添加设备”</p>
      : <Details label="加入一台电脑"><p className="text-meta text-muted-foreground">在那台电脑上运行 <code>vibeface join</code> 加这条链接。</p></Details>}
  </Panel>

  return <main className="h-[var(--app-height,100dvh)] overflow-y-auto overscroll-contain">
    <div className="mx-auto grid w-full max-w-md gap-6 px-4 py-10 pb-[max(2.5rem,env(safe-area-inset-bottom))] sm:py-16">
      <header className="grid gap-3">
        <span className="text-heading font-semibold tracking-tight">vibeface</span>
        {standalone() && !device.entries.length && !focused ? <p className="text-meta text-muted-foreground">主屏幕上的 vibeface 需要单独加入一次</p> : null}
        {step ? <Steps step={step} /> : null}
      </header>
      {linking && !focused ? <p role="status" className="flex items-center gap-2 text-meta text-info"><LoaderCircle aria-hidden="true" className="size-4 animate-spin" />{device.phase === 'reconnecting' ? '正在重连' : '连接中'}</p> : null}
      {alert}
      {device.phase === 'initializing' ? <Skeleton className="h-48" /> : body}
      {other}
      {device.identity && !focused ? <Details key={step ?? active}>
        <DetailRow label="这个浏览器的设备编号" value={device.identity.deviceId} mono />
        {usable ? <form className="grid gap-3 border-t pt-3" onSubmit={(event) => { event.preventDefault(); const form = event.currentTarget; void run(() => device.connect(parseEntry({ address: value(form, 'address'), device_id: value(form, 'device_id') }))) }}>
          <fieldset disabled={linking} className="grid gap-3">
            <legend className="sr-only">手填访问地址</legend>
            <Field label="访问地址"><Input type="url" name="address" required defaultValue={typeof location === 'undefined' ? '' : location.origin} /></Field>
            <Field label="设备编号"><Input className="font-mono" name="device_id" required pattern="[a-f0-9]{64}" autoComplete="off" autoCapitalize="none" spellCheck={false} /></Field>
            <Button type="submit" variant="outline" className="w-fit">连接</Button>
          </fieldset>
        </form> : null}
      </Details> : null}
    </div>
  </main>
}

function NamesStep({ onNext }: { onNext(names: { computer: string; browser: string }): void }) {
  return <Panel icon={Laptop} title="给设备起名">
    <form className="grid gap-4" onSubmit={(event) => { event.preventDefault(); const form = event.currentTarget; onNext({ computer: value(form, 'computer').trim(), browser: value(form, 'browser').trim() }) }}>
      <Field label="这台电脑叫什么？"><Input name="computer" required maxLength={128} autoComplete="off" defaultValue={defaultComputerName()} /></Field>
      <Field label="这个浏览器叫什么？"><Input name="browser" required maxLength={128} autoComplete="off" defaultValue={defaultBrowserName()} /></Field>
      <Button type="submit" className="w-fit justify-self-end">下一步</Button>
    </form>
  </Panel>
}

type Run = (action: () => Promise<void>) => Promise<void>

/** Step 2a: the recovery password; the file is sealed with it right away. */
function PasswordStep({ busy, disabled, names, onBack, run }: { busy: boolean; disabled: boolean; names: { computer: string; browser: string }; onBack(): void; run: Run }) {
  const device = useDevice()
  const [show, setShow] = useState(false)
  const [short, setShort] = useState(false)
  return <Panel icon={FileKey} title="设置恢复">
    <p className="text-muted-foreground">设备都丢了时，用恢复文件和密码找回。</p>
    <form className="grid gap-4" noValidate onSubmit={(event) => {
      event.preventDefault()
      const form = event.currentTarget
      const input = form.elements.namedItem('password') as HTMLInputElement
      if ([...input.value].length < 12) { setShort(true); input.focus(); return }
      void run(async () => {
        try { await device.createLocalNetwork(names.browser, passphrase(value(form, 'password'), value(form, 'password')), names.computer) }
        finally { form.reset() }
      })
    }}>
      <fieldset disabled={busy || disabled} className="grid gap-4">
        <Field label="恢复密码">
          <span className="flex gap-2">
            <Input type={show ? 'text' : 'password'} name="password" autoComplete="new-password" aria-invalid={short || undefined} aria-describedby="password-hint" onChange={() => setShort(false)} />
            <Button type="button" variant="ghost" size="icon" aria-label={show ? '隐藏密码' : '显示密码'} onClick={() => setShow(!show)}>{show ? <EyeOff aria-hidden="true" /> : <Eye aria-hidden="true" />}</Button>
          </span>
          <span id="password-hint" className={cn('font-normal', short ? 'text-danger' : 'text-muted-foreground')}>至少 12 个字符，忘了就无法找回</span>
        </Field>
        <div className="flex justify-between gap-2">
          <Button type="button" variant="ghost" className="-ms-3" onClick={onBack}>上一步</Button>
          <Button type="submit">{busy ? <LoaderCircle aria-hidden="true" className="animate-spin" /> : null}下一步</Button>
        </div>
      </fieldset>
    </form>
  </Panel>
}

/** Step 2b: download the sealed file, confirm it is stored apart from the password, then create. */
function RecoveryStep({ busy, run }: { busy: boolean; run: Run }) {
  const device = useDevice()
  const [downloaded, setDownloaded] = useState(false)
  const [saved, setSaved] = useState(false)
  return <Panel icon={FileKey} title="保存恢复文件">
    <Button type="button" variant="outline" className="w-fit" disabled={busy} onClick={() => void run(async () => { await device.downloadBackup(); setDownloaded(true) })}>{downloaded ? <><CircleCheck aria-hidden="true" className="text-success" />已下载</> : '下载恢复文件'}</Button>
    <label className={cn('flex min-h-11 items-center gap-2', !downloaded && 'text-muted-foreground')}><input type="checkbox" className="size-4" disabled={!downloaded || busy} checked={saved} onChange={(event) => setSaved(event.target.checked)} />已把文件和密码分开保存</label>
    <div className="flex justify-between gap-2">
      <Button type="button" variant="ghost" className="-ms-3" disabled={busy} onClick={device.cancelSensitive}>上一步</Button>
      <Button type="button" disabled={!saved || busy} onClick={() => void run(async () => { startOnboarding(); await device.confirmBootstrap(true) })}>{busy ? <LoaderCircle aria-hidden="true" className="animate-spin" /> : null}完成创建</Button>
    </div>
  </Panel>
}

/** A browser joining through a link: it shows the safety code and waits for an admin to allow it. */
function BrowserJoin() {
  const device = useDevice()
  const joining = device.joining!
  return <Panel icon={Hourglass} title="等待批准">
    {device.info ? <p className="flex items-center gap-2 text-success"><ShieldCheck aria-hidden="true" className="size-4 shrink-0" />已确认是你的设备「{device.info.name}」</p> : null}
    <p className="text-muted-foreground">在发链接的设备上比对安全码</p>
    {joining.code ? <SafetyCode code={joining.code} /> : <Skeleton className="h-8 w-40" />}
    {device.phase === 'reconnecting' ? <p role="status" className="flex items-center gap-2 text-meta text-info"><LoaderCircle aria-hidden="true" className="size-4 animate-spin" />正在重连，请求还在</p> : null}
    <Button type="button" variant="ghost" className="-ms-3 w-fit" onClick={device.cancelJoin}>取消</Button>
    <Details>
      <DetailRow label="这个浏览器" value={device.identity?.deviceId ?? ''} mono />
      <DetailRow label="加入请求" value={joining.join_id} mono />
    </Details>
  </Panel>
}
