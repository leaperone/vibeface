import { useEffect, useRef, useState, type ReactNode } from 'react'
import { ArchiveRestore, FileKey, KeyRound } from 'lucide-react'
import { BusyIcon, Details, DetailRow, FilePicker, InlineError, toast } from '@/components/app/kit'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import type { Member } from '@/lib/node'
import { messageOf, parseEntry } from '@/lib/node'
import { passphrase, recoveryCode, recoveryFileStem } from '@/lib/secure'
import { defaultBrowserName } from '@/lib/device-name'
import { useDevice } from './context'
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

/** The file that still works: the code in its name and the day it was made. */
export function CurrentRecoveryFile() {
  const { directory, recoverySince } = useDevice()
  const key = directory?.recovery_key
  const [code, setCode] = useState<{ key: string; value: string } | null>(null)
  useEffect(() => {
    if (!key) return
    let live = true
    void recoveryCode(key).then((value) => { if (live) setCode({ key, value }) })
    return () => { live = false }
  }, [key])
  if (!key || code?.key !== key) return null
  return <p className="flex min-w-0 flex-wrap items-center gap-x-3 gap-y-1 rounded-lg bg-muted/60 px-3 py-2 text-meta">
    <span className="flex min-w-0 items-center gap-2"><FileKey aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" /><span className="sr-only">有效的文件 </span><span className="min-w-0 font-mono [overflow-wrap:anywhere]">{recoveryFileStem(code.value)}</span></span>
    {recoverySince ? <time className="text-muted-foreground tabular-nums" dateTime={new Date(recoverySince).toISOString()}>{new Date(recoverySince).toLocaleDateString()}</time> : null}
    <span className="text-muted-foreground">只有这份有效，丢了不能补</span>
  </p>
}

/** After unlocking a recovery file: keep every device except the lost ones. */
export function RecoveryReview() {
  const device = useDevice()
  const review = device.review
  const { busy, error, run } = useGenerationRun()
  const [lost, setLost] = useState<string[]>([])
  const [admins, setAdmins] = useState<string[] | null>(null)
  if (review?.kind !== 'recovery') return null
  const managers = admins ?? review.members.filter((member) => member.manage).map((member) => member.device_id)
  const toggle = (list: string[], id: string, on: boolean) => on ? [...list, id] : list.filter((item) => item !== id)
  return <section className="grid gap-4 rounded-xl border bg-card shadow-xs p-5">
    <h2 className="flex items-center gap-2 text-heading font-semibold"><ArchiveRestore aria-hidden="true" className="size-5 text-muted-foreground" />哪台设备丢了？</h2>
    <fieldset disabled={busy} className="grid gap-2">
      {[...review.members].sort((a, b) => Number(b.device_id === review.requiredNode) - Number(a.device_id === review.requiredNode)).map((member) => {
        const required = member.device_id === review.requiredNode
        const Icon = deviceIcon({ kind: member.kind, name: member.name, entry: required })
        return <div key={member.device_id} className="grid gap-1 rounded-lg bg-muted/60 px-3 py-2">
          <div className="flex min-h-9 items-center gap-2">
            <Icon aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" />
            <span className="min-w-0 flex-1 truncate">{member.name}</span>
            {required ? <span className="text-meta text-muted-foreground">当前连接</span>
              : <label className="-me-2 flex min-h-11 items-center gap-2 px-2 text-meta"><input type="checkbox" className="size-4" checked={lost.includes(member.device_id)} onChange={(event) => setLost(toggle(lost, member.device_id, event.target.checked))} />丢了</label>}
          </div>
          {member.kind === 'client' && !lost.includes(member.device_id) ? <label className="flex min-h-11 w-fit items-center gap-2 ps-6 pe-2 text-meta text-muted-foreground"><input type="checkbox" className="size-4" checked={managers.includes(member.device_id)} onChange={(event) => setAdmins(toggle(managers, member.device_id, event.target.checked))} />管理员</label> : null}
        </div>
      })}
    </fieldset>
    <p className="text-meta text-muted-foreground">这个浏览器会成为管理员，并下载新的恢复文件</p>
    <InlineError error={error || null} />
    <div className="flex flex-wrap justify-between gap-2">
      <Button type="button" variant="ghost" onClick={device.cancelSensitive}>取消</Button>
      <Button type="button" disabled={busy} onClick={() => void run(async () => {
        await device.confirmRecovery(review.members
          .filter((member) => member.device_id === review.requiredNode || !lost.includes(member.device_id))
          .map((member): Member => ({ ...member, manage: member.kind === 'client' && managers.includes(member.device_id) })))
        toast('已恢复。旧恢复文件已失效，请保存刚下载的新文件', { tone: 'warning', persist: true })
      })}><BusyIcon busy={busy} />恢复</Button>
    </div>
    {busy ? <p className="text-meta text-muted-foreground">如果已经提交，取消不会撤回；重新连接后看一下设备列表。</p> : null}
    <Details><DetailRow label="个人网络" value={review.networkId} mono /></Details>
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
  if (!open) return <div className="flex flex-wrap gap-2">
    <Button type="button" variant="outline" onClick={() => setOpen('restore')}><ArchiveRestore aria-hidden="true" />用恢复文件找回</Button>
    <Button type="button" variant="outline" onClick={() => setOpen('passphrase')}><KeyRound aria-hidden="true" />换恢复密码</Button>
  </div>
  return <div className="grid gap-4">
    {open === 'restore' ? <section className="grid gap-4 rounded-xl border bg-card shadow-xs p-5">
      <h2 className="flex items-center gap-2 text-heading font-semibold"><ArchiveRestore aria-hidden="true" className="size-5 text-muted-foreground" />用恢复文件找回</h2>
      <p className="text-meta text-muted-foreground">只找回设备和管理权限，不含对话</p>
      <form className="grid gap-4" onSubmit={(event) => {
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
        <div className="flex flex-wrap gap-2"><Button type="submit"><BusyIcon busy={busy} />下一步</Button>{!only ? <Button type="button" variant="ghost" onClick={() => setOpen(undefined)}>取消</Button> : null}</div>
      </fieldset></form>
    </section> : null}
    {open === 'passphrase' ? <section className="grid gap-4 rounded-xl border bg-card shadow-xs p-5">
      <h2 className="flex items-center gap-2 text-heading font-semibold"><KeyRound aria-hidden="true" className="size-5 text-muted-foreground" />换恢复密码</h2>
      <p className="text-meta text-muted-foreground">换完后旧文件失效</p>
      <form className="grid gap-4" onSubmit={(event) => {
        event.preventDefault(); const form = event.currentTarget
        void run(async () => {
          let work: Promise<void>
          try { work = device.replaceRecoveryFile({ file: fileOf(form), oldPassword: field(form, 'old_password'), password: passphrase(field(form, 'password'), field(form, 'confirmation')) }) }
          finally { clearPasswords(form) }
          await work
        }, '已下载新文件，旧恢复文件已失效')
      }}><fieldset disabled={busy || !!device.review} className="grid gap-4">
        <div className="grid gap-1.5 text-meta font-medium">恢复文件<FilePicker name="bundle" accept="application/json,.json" required /></div>
        <Label label="旧密码"><Input type="password" name="old_password" autoComplete="off" required /></Label>
        <Label label="新密码（至少 8 个字符）"><Input type="password" name="password" minLength={8} autoComplete="new-password" required /></Label>
        <Label label="再输一次"><Input type="password" name="confirmation" minLength={8} autoComplete="new-password" required /></Label>
        <InlineError error={error || null} />
        <div className="flex flex-wrap gap-2"><Button type="submit" variant="outline"><BusyIcon busy={busy} />下载新文件</Button>{!only ? <Button type="button" variant="ghost" onClick={() => setOpen(undefined)}>取消</Button> : null}</div>
      </fieldset></form>
    </section> : null}
    {status ? <p role="status" className="text-meta text-success">{status}</p> : null}
  </div>
}
