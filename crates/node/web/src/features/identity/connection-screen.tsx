import { useEffect, useRef, useState, type FormEvent, type ReactNode } from 'react'
import { ArchiveRestore, Eye, EyeOff, FileKey, Hourglass, Laptop, Link2, LoaderCircle, MonitorSmartphone, ShieldAlert, Upload, type LucideIcon } from 'lucide-react'
import { cn } from 'cn'
import { Details, DetailRow, EmptyState, InlineError, SafetyCode, Skeleton } from '@/components/app/kit'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Textarea } from '@/components/ui/textarea'
import { useCheckCode } from '@/lib/check-code'
import { addressName } from '@/features/workspace/labels'
import { readInvitation } from '../../../transport.js'
import { messageOf, parseEntry, parseInvitation } from '@/lib/node'
import { readConnectionFile } from '@/lib/connection-file'
import { passphrase } from '@/lib/secure'
import { defaultBrowserName, defaultComputerName } from '@/lib/device-name'
import { startOnboarding } from './onboarding-flag'
import { primaryLane } from './connection-lane'
import { useDevice } from './context'
import { RecoveryControls, RecoveryReview } from './recovery-controls'

type Lane = 'local' | 'invite' | 'saved' | 'recovery'
type Purpose = 'empty' | 'bootstrap' | 'pair' | 'node' | 'invalid'
const value = (form: HTMLFormElement, key: string) => { const field = form.elements.namedItem(key); return field instanceof HTMLInputElement || field instanceof HTMLTextAreaElement ? field.value : '' }

function purposeOf(invitation: string): Purpose {
  if (!invitation.trim()) return 'empty'
  try { return parseInvitation(readInvitation(invitation.trim())).kind } catch { return 'invalid' }
}


function Steps({ step }: { step: number }) {
  return <div className="flex items-center gap-3 text-meta text-muted-foreground">
    <span className="flex gap-1.5" aria-hidden="true">{[1, 2, 3, 4].map((item) => <span key={item} className={cn('size-2 rounded-full', item <= step ? 'bg-foreground' : 'bg-muted-foreground/30')} />)}</span>
    第 {step} 步，共 4 步
  </div>
}

function Panel({ icon: Icon, title, children }: { icon: LucideIcon; title: string; children: ReactNode }) {
  return <section className="grid gap-4 rounded-xl border bg-card p-5">
    <h2 className="flex items-center gap-2 text-heading font-semibold"><Icon aria-hidden="true" className="size-5 text-muted-foreground" />{title}</h2>
    {children}
  </section>
}

function Field({ label, children }: { label: string; children: ReactNode }) {
  return <label className="grid gap-1.5 text-meta font-medium">{label}{children}</label>
}

export function ConnectionScreen() {
  const device = useDevice()
  const generation = device.connectionGeneration()
  const [busyGeneration, setBusyGeneration] = useState<number | null>(null)
  const busy = busyGeneration === generation
  const pending = useRef<number | null>(null)
  const [error, setError] = useState('')
  const [invitation, setInvitation] = useState(device.initialInvitation)
  const [lane, setLane] = useState<Lane | null>(null)
  const [names, setNames] = useState<{ computer: string; browser: string } | null>(null)
  const suggested = primaryLane({ hasInvitation: Boolean(device.initialInvitation), savedCount: device.entries.length, hasLocalSetup: Boolean(device.localSetup) })
  const active = lane ?? suggested
  const purpose = purposeOf(invitation)
  const usable = !!device.identity && !['blocked', 'key_lost', 'initializing'].includes(device.phase)
  const linking = busy || ['connecting', 'reconnecting'].includes(device.phase)
  useEffect(() => { if (device.initialInvitation) setInvitation((current) => current.trim() ? current : device.initialInvitation) }, [device.initialInvitation])
  useEffect(() => { setError('') }, [generation])
  useEffect(() => { if (device.localJoin?.kind === 'joined') setLane('local') }, [device.localJoin])

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
    void run(() => device.pair(invitation, value(form, 'name'), purpose === 'bootstrap' ? passphrase(value(form, 'password'), value(form, 'confirmation')) : undefined))
  }
  async function importFile(file: File) {
    const owner = device.connectionGeneration()
    const content = await readConnectionFile(file)
    if (owner !== device.connectionGeneration()) return
    const parsed = readInvitation(content)
    parseInvitation(parsed)
    setInvitation(JSON.stringify(parsed))
  }

  // Not being a member yet is the normal state on this screen, so it reads as a hint rather than an error.
  const alert = error || device.error && !['reconnecting', 'unpaired'].includes(device.phase) ? <InlineError error={error || device.error} />
    : device.error && device.phase === 'unpaired' ? <p role="status" className="text-meta text-muted-foreground">{device.error}</p> : null
  const joining = device.localJoin && device.localJoin.kind !== 'idle' && device.localJoin.kind !== 'joined'
  const focused = Boolean(device.review || device.waiting || joining)
  const setup = device.localSetup?.kind === 'uninitialized'
  const other = !focused && usable ? <div className="grid gap-2">
    <p className="text-meta text-muted-foreground">其他方式</p>
    <div className="flex flex-wrap gap-2">
      {active !== 'local' && device.localSetup ? <Button variant="ghost" type="button" onClick={() => chooseLane('local')}><Laptop aria-hidden="true" />{setup ? '设置这台电脑' : '这台电脑'}</Button> : null}
      {active !== 'saved' && device.entries.length > 0 ? <Button variant="ghost" type="button" onClick={() => chooseLane('saved')}><MonitorSmartphone aria-hidden="true" />用过的地址</Button> : null}
      {active !== 'invite' ? <Button variant="ghost" type="button" onClick={() => chooseLane('invite')}><Link2 aria-hidden="true" />已有设备？用加入链接</Button> : null}
      {active !== 'recovery' ? <Button variant="ghost" type="button" onClick={() => chooseLane('recovery')}><ArchiveRestore aria-hidden="true" />用恢复文件找回</Button> : null}
    </div>
  </div> : null

  let body: ReactNode
  if (device.phase === 'blocked') body = <EmptyState icon={ShieldAlert} title="请用本机地址或 HTTPS 打开" />
  else if (device.phase === 'key_lost') body = <Panel icon={ShieldAlert} title="需要重新加入">
    <p className="text-muted-foreground">这个浏览器保存的登录信息不见了。</p>
    <Button type="button" className="w-fit" disabled={busy} onClick={() => void run(device.replaceLostKey)}>重新开始</Button>
  </Panel>
  else if (device.review?.kind === 'bootstrap') body = <RecoveryStep busy={busy} run={run} />
  else if (device.review) body = <RecoveryReview />
  else if (joining && device.localJoin) body = <NodeJoin busy={busy} run={run} />
  else if (device.waiting) body = <BrowserJoin busy={busy} run={run} />
  else if (active === 'local' && setup && !names) body = <NamesStep onNext={setNames} />
  else if (active === 'local' && setup && names) body = <PasswordStep busy={busy || linking} disabled={!usable} names={names} onBack={() => setNames(null)} run={run} />
  else if (active === 'local' && device.localSetup) body = <Panel icon={Laptop} title={device.localJoin?.kind === 'joined' ? '这台电脑已加入' : '这台电脑'}>
    {device.localSetup.kind === 'initialized' && device.localSetup.local_launch
      ? <Button type="button" className="w-fit" disabled={linking || !usable} onClick={() => void run(device.openLocalAccess)}>打开这台电脑</Button>
      : <p className="text-muted-foreground">用已有设备的加入链接，或恢复文件</p>}
  </Panel>
  else if (active === 'saved') body = <Panel icon={MonitorSmartphone} title="用过的地址">
    <ul className="grid gap-2">{device.entries.map((entry) => <li key={`${entry.address}:${entry.peer_id}:${entry.local === true}`} className="flex items-center gap-3 rounded-lg bg-muted/60 px-3 py-2">
      <span className="min-w-0 flex-1 truncate" title={entry.address}>{addressName(entry.address)}</span>
      <Button type="button" disabled={linking || !usable} onClick={() => void run(() => device.connect(entry))}>连接</Button>
    </li>)}</ul>
  </Panel>
  else if (active === 'recovery') body = usable ? <RecoveryControls only="restore" /> : null
  else body = <Panel icon={Link2} title={purpose === 'node' ? '把这台电脑加入' : '加入我的设备'}>
    <form className="grid gap-4" onSubmit={submitInvitation}>
      <fieldset disabled={linking || !usable} className="grid gap-4">
        <Field label="加入链接"><Textarea className="min-h-20 font-mono text-meta" name="invitation" required value={invitation} aria-invalid={purpose === 'invalid' || undefined} autoComplete="off" spellCheck={false} placeholder="粘贴在已有设备上复制的链接" onChange={(event) => setInvitation(event.target.value)} /></Field>
        <label className="flex w-fit cursor-pointer items-center gap-2 text-meta text-muted-foreground hover:text-foreground"><Upload aria-hidden="true" className="size-4" />或导入文件
          <input className="sr-only" type="file" accept=".json,application/json,text/plain" onChange={(event) => { const file = event.currentTarget.files?.item(0); event.currentTarget.value = ''; if (file) void run(() => importFile(file)) }} />
        </label>
        <Field label={purpose === 'node' ? '这台电脑叫什么？' : '这个浏览器叫什么？'}><Input name="name" required maxLength={128} autoComplete="off" defaultValue={purpose === 'node' ? defaultComputerName() : defaultBrowserName()} key={purpose === 'node' ? 'node' : 'browser'} /></Field>
        {purpose === 'bootstrap' ? <>
          <Field label="恢复密码"><Input type="password" name="password" autoComplete="new-password" minLength={12} required /></Field>
          <Field label="再输一次"><Input type="password" name="confirmation" autoComplete="new-password" minLength={12} required /></Field>
        </> : null}
        <Button type="submit" className="w-fit">{linking ? <LoaderCircle aria-hidden="true" className="animate-spin" /> : null}继续</Button>
      </fieldset>
    </form>
    {purpose === 'node' && device.localSetup ? <Button variant="ghost" type="button" className="w-fit" disabled={linking || !usable} onClick={() => void run(device.refreshLocalJoin)}>再查一次</Button> : null}
  </Panel>

  return <main className="h-[var(--app-height,100dvh)] overflow-y-auto overscroll-contain">
    <div className="mx-auto grid w-full max-w-md gap-6 px-4 py-10 pb-[max(2.5rem,env(safe-area-inset-bottom))] sm:py-16">
      <header className="grid gap-3">
        <span className="text-heading font-semibold tracking-tight">vibeface</span>
        {active === 'local' && setup && !focused ? <Steps step={names ? 2 : 1} /> : device.review?.kind === 'bootstrap' ? <Steps step={2} /> : null}
      </header>
      {linking && !focused ? <p role="status" className="flex items-center gap-2 text-meta text-info"><LoaderCircle aria-hidden="true" className="size-4 animate-spin" />{device.phase === 'reconnecting' ? '正在重连' : '连接中'}</p> : null}
      {alert}
      {device.phase === 'initializing' ? <Skeleton className="h-48" /> : body}
      {other}
      {!focused && usable ? <Details label="手填访问地址">
        <form className="grid gap-3" onSubmit={(event) => { event.preventDefault(); const form = event.currentTarget; void run(() => device.connect(parseEntry({ address: value(form, 'address'), peer_id: value(form, 'peer_id') }))) }}>
          <fieldset disabled={linking} className="grid gap-3">
            <Field label="访问地址"><Input type="url" name="address" required defaultValue={typeof location === 'undefined' ? '' : location.origin} /></Field>
            <Field label="设备编号"><Input className="font-mono" name="peer_id" required pattern="[a-f0-9]{64}" autoComplete="off" autoCapitalize="none" spellCheck={false} /></Field>
            <Button type="submit" variant="outline" className="w-fit">连接</Button>
          </fieldset>
        </form>
      </Details> : null}
      {device.identity && !focused ? <Details><DetailRow label="这个浏览器的设备编号" value={device.identity.peerId} mono /></Details> : null}
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
  return <Panel icon={FileKey} title="设置恢复">
    <p className="text-muted-foreground">设备都丢了时，用恢复文件和密码找回。</p>
    <form className="grid gap-4" onSubmit={(event) => {
      event.preventDefault()
      const form = event.currentTarget
      void run(async () => {
        try { await device.createLocalNetwork(names.browser, passphrase(value(form, 'password'), value(form, 'password')), names.computer) }
        finally { form.reset() }
      })
    }}>
      <fieldset disabled={busy || disabled} className="grid gap-4">
        <Field label="恢复密码">
          <span className="flex gap-2">
            <Input type={show ? 'text' : 'password'} name="password" autoComplete="new-password" minLength={12} required />
            <Button type="button" variant="ghost" size="icon" aria-label={show ? '隐藏密码' : '显示密码'} onClick={() => setShow(!show)}>{show ? <EyeOff aria-hidden="true" /> : <Eye aria-hidden="true" />}</Button>
          </span>
          <span className="font-normal text-muted-foreground">至少 12 个字符，忘了就无法找回</span>
        </Field>
        <div className="flex justify-between gap-2">
          <Button type="button" variant="ghost" onClick={onBack}>上一步</Button>
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
    <p className="text-muted-foreground">设备都丢了时，用它找回。</p>
    <Button type="button" variant="outline" className="w-fit" disabled={busy} onClick={() => void run(async () => { device.downloadBackup(); setDownloaded(true) })}>下载恢复文件</Button>
    <label className={cn('flex min-h-11 items-center gap-2', !downloaded && 'text-muted-foreground')}><input type="checkbox" className="size-4" disabled={!downloaded || busy} checked={saved} onChange={(event) => setSaved(event.target.checked)} />已把文件和密码分开保存</label>
    <div className="flex justify-between gap-2">
      <Button type="button" variant="ghost" disabled={busy} onClick={device.cancelSensitive}>取消</Button>
      <Button type="button" disabled={!saved || busy} onClick={() => void run(async () => { startOnboarding(); await device.confirmBootstrap(true) })}>{busy ? <LoaderCircle aria-hidden="true" className="animate-spin" /> : null}完成创建</Button>
    </div>
  </Panel>
}

/** A browser joining through a link: compare the check code, then wait for an admin. */
function BrowserJoin({ busy, run }: { busy: boolean; run: Run }) {
  const device = useDevice()
  const waiting = device.waiting!
  const code = useCheckCode(waiting.network_id, waiting.review.inviter_subject_id, waiting.review.new_member.subject_id)
  useEffect(() => {
    if (!waiting.accepted) return
    const timer = setInterval(() => { device.checkPair().catch(() => {}) }, 5000)
    return () => clearInterval(timer)
  }, [waiting.accepted, waiting.request_id])
  return <Panel icon={waiting.accepted ? Hourglass : MonitorSmartphone} title={waiting.accepted ? '等待批准' : '对一下核对码'}>
    <p className="text-muted-foreground">{waiting.accepted ? '在已有设备上点“允许”后，这里会自动进入。' : '在已有设备的“我的设备”里能看到同一组核对码。'}</p>
    {code ? <SafetyCode code={code} /> : <Skeleton className="h-8 w-56" />}
    <div className="flex flex-wrap justify-between gap-2">
      <Button type="button" variant="ghost" onClick={() => device.cancelPair()}>取消</Button>
      {waiting.accepted
        ? <Button type="button" variant="outline" disabled={busy} onClick={() => void run(device.checkPair)}>再查一次</Button>
        : <Button type="button" disabled={busy || !code} onClick={() => void run(device.acceptPair)}>核对码一致</Button>}
    </div>
    <Details>
      <DetailRow label="这个浏览器" value={waiting.review.new_member.subject_id} mono />
      <DetailRow label="邀请方" value={waiting.review.inviter_subject_id} mono />
      <DetailRow label="个人网络" value={waiting.network_id} mono />
    </Details>
  </Panel>
}

/** A computer joining through a link opened on its own local page. */
function NodeJoin({ busy, run }: { busy: boolean; run: Run }) {
  const device = useDevice()
  const join = device.localJoin!
  const review = join.kind === 'review' || join.kind === 'pending' ? join.review : null
  const code = useCheckCode(review?.network_id ?? '', review?.inviter_subject_id ?? '', review?.new_member.subject_id ?? '')
  return <Panel icon={join.kind === 'pending' ? Hourglass : Laptop} title={join.kind === 'review' ? '对一下核对码' : join.kind === 'pending' ? '等待批准' : '还不确定是否已加入'}>
    {review ? <>
      <p className="text-muted-foreground">{join.kind === 'review' ? '在已有设备的“我的设备”里能看到同一组核对码。' : '在已有设备上点“允许”后再查一次。'}</p>
      {code ? <SafetyCode code={code} /> : <Skeleton className="h-8 w-56" />}
    </> : <p className="text-muted-foreground">再查一次，不会重复提交。</p>}
    <div className="flex flex-wrap justify-between gap-2">
      <Button type="button" variant="ghost" disabled={busy} onClick={device.useAnotherNodeInvitation}>换一个链接</Button>
      {join.kind === 'review'
        ? <Button type="button" disabled={busy || !code || join.review.expires_at_ms <= Date.now()} onClick={() => void run(device.acceptLocalJoin)}>核对码一致</Button>
        : <Button type="button" variant="outline" disabled={busy} onClick={() => void run(device.refreshLocalJoin)}>再查一次</Button>}
    </div>
    {review ? <Details>
      <DetailRow label="这台电脑" value={review.new_member.subject_id} mono />
      <DetailRow label="邀请方" value={review.inviter_subject_id} mono />
      <DetailRow label="个人网络" value={review.network_id} mono />
    </Details> : null}
  </Panel>
}
