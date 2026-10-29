import { useEffect, useRef, useState, type ReactNode } from 'react'
import { ArrowDownLeft, ArrowDownToLine, ArrowRightLeft, ArrowUpRight, CircleCheck, Copy, File, FlaskConical, RefreshCw, type LucideIcon } from 'lucide-react'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { useDelivery } from '@vibeface/client/delivery/client.ts'
import { acceptanceOptions, canRestart, changeMode, deadlineRows, draftFor, evidenceLabels, modeOptions, orderActions, orderHeading, orderStatus, orderSummary, orderTitle, receiverChoices, settlementAmountsLabel, sizeLabel, transferCopy, walletNotice } from '@vibeface/client/delivery/model.ts'
import type { DeliveryDraft, DeliveryOrder, DeliverySettings, TransferFile } from '@vibeface/client/delivery/types.ts'
import { messageOf } from '@vibeface/client/lib/node.ts'
import { useExperiments } from '@vibeface/client/lib/experiments.ts'
import { BusyIcon, Details, EmptyState, InlineError, Segmented, useAction } from '@/components/app/kit'
import { Button } from '@/components/ui/button'
import { Field, FieldDescription, FieldLabel } from '@/components/ui/field'
import { Input } from '@/components/ui/input'
import { NativeSelect, NativeSelectOption } from '@/components/ui/native-select'
import { terms } from '@/lib/glossary'
import { webPlatform } from '@/lib/platform'

const roleIcons = { sender: ArrowUpRight, receiver: ArrowDownLeft, provider: ArrowRightLeft } as const

export function Transfers() {
  const { ready } = useDevice()
  return ready ? <TransferPage key={`${ready.generation}:${ready.entry.device_id}`} connection={ready} /> : <EmptyState icon={ArrowRightLeft} title="设备暂时离线" description={transferCopy.offline} />
}

function TransferPage({ connection }: { connection: NonNullable<ReturnType<typeof useDevice>['ready']> }) {
  const delivery = useDelivery(connection, webPlatform)
  const { directory } = useDevice()
  // Paid delivery and its test chain stay out of sight unless 实验功能 is on in settings.
  const [experiments] = useExperiments(webPlatform.storage)
  const [draft, setDraft] = useState<DeliveryDraft>({ receiver_peer: '', provider_peer: '', mode: 'charity', acceptance: 'recoverable', max_service_amount: '0', max_gas_amount: '0' })
  const [file, setFile] = useState<TransferFile | null>(null)
  // Feedback shows next to the control that caused it, not at the top of a long page.
  const [origin, setOrigin] = useState<'page' | 'form' | 'order'>('page')
  const picker = useAction()
  const status = delivery.status
  const selected = delivery.selected
  const working = delivery.busy !== null || picker.busy !== null
  const change = (patch: Partial<DeliverySettings>) => {
    setOrigin('page')
    if (status) void delivery.configure({ enabled: status.enabled, charity_enabled: status.charity_enabled, paid_enabled: status.paid_enabled, ...patch })
  }
  const feedback = (where: typeof origin) => origin === where ? <Feedback error={delivery.error} notice={delivery.notice} /> : null
  // Paid already on at this Node stays visible, so it can be seen and turned off.
  const paid = experiments || !!status?.paid_enabled
  const sending = draftFor(draft, paid)
  const mine = receiverChoices(directory?.members ?? [], status?.node_peer ?? '')
  const picked = mine.some(item => item.device_id === draft.receiver_peer)
  const receiverId = <Field><FieldLabel htmlFor="transfer-receiver">{transferCopy.receiver}</FieldLabel><Input id="transfer-receiver" className="font-mono" value={picked ? '' : draft.receiver_peer} autoCapitalize="none" autoCorrect="off" spellCheck={false} maxLength={64} required={!picked} disabled={working} onChange={event => setDraft({ ...draft, receiver_peer: event.target.value })} /></Field>
  const unusable = working || !file || !draft.receiver_peer || status?.max_file_bytes === 0 || sending.mode === 'charity' && !status?.charity_enabled || sending.mode === 'paid' && !status?.paid_enabled
  return <div className="mx-auto grid w-full max-w-(--page-width) gap-6">
    {feedback('page')}
    {!status ? <p role="status" className="text-meta text-muted-foreground">{transferCopy.loading}</p> : <>
      <section aria-labelledby="transfer-settings" className="grid gap-4 rounded-xl border bg-card p-4">
        <h2 id="transfer-settings" className="text-heading font-semibold">传输设置</h2>
        <div className="grid">
          <Toggle label="启用文件传输" checked={status.enabled} disabled={!delivery.canManage || working} change={enabled => change({ enabled })} />
          {status.enabled ? <>
            <Toggle label={transferCopy.charity} checked={status.charity_enabled} disabled={!delivery.canManage || working} change={charity_enabled => change({ charity_enabled })} />
            {paid ? <Toggle label={transferCopy.paid} icon={FlaskConical} checked={status.paid_enabled} disabled={!delivery.canManage || working} change={paid_enabled => change({ paid_enabled })} /> : null}
          </> : null}
        </div>
        {status.enabled ? <ConnectionId value={status.node_peer} /> : <p className="text-meta text-muted-foreground">{transferCopy.off}</p>}
        {!delivery.canManage ? <p className="flex items-center gap-1.5 text-meta text-muted-foreground"><terms.admin.icon aria-hidden="true" className="size-4 shrink-0" />{transferCopy.adminOnly}</p> : null}
        {paid ? <>
          {status.paid_enabled || status.wallet.state !== 'absent' ? <p className="text-meta text-muted-foreground">{walletNotice(status.wallet)}</p> : null}
          {status.wallet.state === 'unavailable' ? <p role="alert" className="text-meta text-warning">{status.wallet.reason}</p> : null}
          <Details>
            {status.wallet.address ? <Info label="设备钱包" value={status.wallet.address} /> : null}
            {status.receiver_key ? <><Info label="当前接收密钥 ID" value={status.receiver_key.key_id} /><Info label="当前接收密钥序号" value={status.receiver_key.serial.toString()} /></> : null}
            <p>{transferCopy.walletDetails}</p>
          </Details>
        </> : null}
      </section>
      {status.enabled && delivery.canManage ? <form className="grid gap-4 rounded-xl border bg-card p-4" onSubmit={event => { event.preventDefault(); if (file) { setOrigin('form'); void delivery.prepare(sending, file) } }}>
        <h2 className="text-heading font-semibold">发送文件</h2>
        {mine.length ? <>
          <Field><FieldLabel htmlFor="transfer-to">{transferCopy.sendTo}</FieldLabel>
            <NativeSelect id="transfer-to" className="w-full" value={picked ? draft.receiver_peer : ''} disabled={working} onChange={event => setDraft({ ...draft, receiver_peer: event.target.value })}>
              <NativeSelectOption value="" disabled>{transferCopy.pickDevice}</NativeSelectOption>
              {mine.map(item => <NativeSelectOption key={item.device_id} value={item.device_id}>{item.name}</NativeSelectOption>)}
            </NativeSelect>
          </Field>
          <Details label={transferCopy.otherDevice} open={!picked && Boolean(draft.receiver_peer) || undefined}>{receiverId}</Details>
        </> : receiverId}
        <Field><FieldLabel htmlFor="transfer-provider">{transferCopy.provider}</FieldLabel><Input id="transfer-provider" className="font-mono" value={draft.provider_peer} autoCapitalize="none" autoCorrect="off" spellCheck={false} maxLength={64} required disabled={working} onChange={event => setDraft({ ...draft, provider_peer: event.target.value })} /><FieldDescription>{transferCopy.providerHint}</FieldDescription></Field>
        {paid ? <Segmented label="传输方式" value={draft.mode} options={modeOptions} labels disabled={working} onChange={mode => setDraft(changeMode(draft, mode))} /> : null}
        {sending.mode === 'paid' ? <>
          <Segmented label="什么时候付款" value={draft.acceptance} options={acceptanceOptions} labels disabled={working} onChange={acceptance => setDraft({ ...draft, acceptance })} />
          <Field><FieldLabel htmlFor="transfer-service-cap">{transferCopy.serviceCap}</FieldLabel><Input id="transfer-service-cap" inputMode="numeric" pattern="[0-9]+" value={draft.max_service_amount} required disabled={working} onChange={event => setDraft({ ...draft, max_service_amount: event.target.value })} /></Field>
          <Field><FieldLabel htmlFor="transfer-gas-cap">{transferCopy.gasCap}</FieldLabel><Input id="transfer-gas-cap" inputMode="numeric" pattern="[0-9]+" value={draft.max_gas_amount} required disabled={working} onChange={event => setDraft({ ...draft, max_gas_amount: event.target.value })} /></Field>
        </> : <p className="flex items-center gap-1.5 text-meta text-muted-foreground"><terms.encrypted.icon aria-hidden="true" className="size-4 shrink-0 text-success" />{transferCopy.free}</p>}
        <div className="flex flex-wrap items-center gap-x-3 gap-y-2">
          <Button type="button" variant="outline" disabled={working} onClick={() => void picker.run('pick', async () => { const picked = await webPlatform.files?.pick(); if (picked) setFile(picked) })}><BusyIcon busy={picker.busy === 'pick'} icon={File} />选择文件</Button>
          <span className="min-w-0 break-all text-meta text-muted-foreground">{file ? <><span className="text-foreground">{file.name}</span> · {sizeLabel(file.size)}</> : transferCopy.maxSize(status.max_file_bytes)}</span>
        </div>
        {picker.error ? <p role="alert" className="text-meta text-danger">{messageOf(picker.error)}</p> : null}
        {file && file.size > status.max_file_bytes ? <p role="alert" className="text-meta text-danger">{transferCopy.tooLarge(status.max_file_bytes)}</p> : null}
        {feedback('form')}
        <div className="flex flex-wrap gap-2">
          <Button type="submit" disabled={unusable}><BusyIcon busy={delivery.busy === 'prepare'} />{transferCopy.prepare(sending.mode)}</Button>
          {selected && canRestart(selected) && (paid || selected.mode === 'charity') ? <Button type="button" variant="outline" disabled={unusable} onClick={() => { if (file) { setOrigin('form'); void delivery.prepare(sending, file, selected.intent_id) } }}>作为新订单再次发送</Button> : null}
        </div>
        <Details><p>{transferCopy.prepareDetails}</p>{sending.mode === 'paid' ? <p>{transferCopy.units}</p> : null}</Details>
      </form> : null}
    </>}
    <section aria-labelledby="transfer-orders" className="grid gap-3">
      <div className="flex items-center justify-between gap-3">
        <h2 id="transfer-orders" className="text-heading font-semibold">传输记录</h2>
        <Button type="button" variant="ghost" size="sm" disabled={working} onClick={() => { setOrigin('page'); void delivery.refresh() }}><BusyIcon busy={delivery.busy === 'refresh'} icon={RefreshCw} />刷新</Button>
      </div>
      {delivery.orders.length ? <ul className="divide-y overflow-hidden rounded-xl border bg-card">{delivery.orders.map(order => {
        const Icon = roleIcons[order.role]
        return <li key={order.order_id}><button type="button" className="interactive-row flex w-full items-center gap-3 px-4 py-3 text-left" aria-pressed={delivery.selected?.order_id === order.order_id} onClick={() => delivery.select(order.order_id)}>
          <Icon aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" />
          <span className="grid min-w-0 gap-0.5">
            <span className="truncate font-medium">{orderTitle(order)}</span>
            <span className="text-meta text-muted-foreground">{orderSummary(order)}</span>
          </span>
        </button></li>
      })}</ul> : <EmptyState icon={ArrowRightLeft} title={transferCopy.noRecords} />}
    </section>
    {selected ? <OrderDetails key={selected.order_id} order={selected} busy={working} canManage={delivery.canManage} walletReady={status?.wallet.state === 'unlocked'} feedback={feedback('order')}
      confirm={() => { setOrigin('order'); void delivery.confirm(selected) }} accept={() => { setOrigin('order'); void delivery.accept(selected) }}
      reconcile={() => { setOrigin('order'); void delivery.reconcile(selected) }} save={() => { setOrigin('order'); void delivery.save(selected) }} /> : null}
  </div>
}

function OrderDetails({ order, busy, canManage, walletReady, feedback, confirm, accept, reconcile, save }: { order: DeliveryOrder; busy: boolean; canManage: boolean; walletReady: boolean; feedback: ReactNode; confirm(): void; accept(): void; reconcile(): void; save(): void }) {
  const heading = useRef<HTMLHeadingElement>(null)
  useEffect(() => { heading.current?.focus() }, [])
  const paid = order.mode === 'paid'
  const can = orderActions(order)
  return <section className="grid gap-4 rounded-xl border bg-card p-4" aria-labelledby="transfer-detail">
    <h2 ref={heading} id="transfer-detail" tabIndex={-1} className="text-heading font-semibold outline-none">{orderHeading(order)}</h2>
    <p>{orderStatus(order)}</p>
    {order.name ? <Info label="文件" value={`${order.name} · ${sizeLabel(order.bytes)}`} /> : null}
    <Info label="传输方式" value={paid ? transferCopy.paid : <span className="inline-flex items-center gap-1.5"><terms.encrypted.icon aria-hidden="true" className="size-4 text-success" />{transferCopy.charity}</span>} />
    {paid ? <>
      <Info label={transferCopy.serviceTotal} value={order.payment.total} />
      <Info label={transferCopy.gasCap} value={order.payment.gas_cap} />
      {order.acceptance === 'strict' ? <p className="text-meta text-warning">{transferCopy.strict}</p> : null}
    </> : null}
    {order.error ? <p role="alert" className="text-meta text-danger">{order.error}</p> : null}
    {feedback}
    <div className="flex flex-wrap gap-2">
      {can.confirm ? <Button type="button" disabled={busy || !canManage || paid && !walletReady} onClick={confirm}>{transferCopy.confirm(order.mode)}</Button> : null}
      {can.accept ? <Button type="button" disabled={busy || !canManage} onClick={accept}>接收文件</Button> : null}
      {can.save ? <Button type="button" variant={order.role === 'receiver' ? 'default' : 'outline'} disabled={busy} onClick={save}><ArrowDownToLine aria-hidden="true" />保存文件</Button> : null}
      <Button type="button" variant="outline" disabled={busy || !canManage} onClick={reconcile}>检查进度</Button>
    </div>
    {paid && order.phase === 'prepared' && !walletReady ? <p className="text-meta text-warning">{transferCopy.walletFirst}</p> : null}
    {order.payment.state === 'unknown' || order.phase === 'reconciling' ? <p className="text-meta text-warning">{transferCopy.unknownPayment}</p> : null}
    <Details>
      <p>{evidenceLabels[order.evidence]}。交付确认不表示真人已阅读。</p>
      <Info label="订单" value={order.order_id} /><Info label="付款意图" value={order.intent_id} />
      <Info label="发送设备" value={order.source_peer} /><Info label="接收设备" value={order.receiver_peer} /><Info label="中转设备" value={order.provider_peer} />
      <Info label="验收条件" value={paid ? order.acceptance === 'strict' ? '接收设备证明实际解密后结算' : '完整密文持有证明与正确密钥包可取后结算' : '公益收据'} />
      {paid ? <>
        <Info label="测试链 ID" value={order.payment.chain_id ?? ''} /><Info label="测试资产合约" value={order.payment.asset ?? ''} /><Info label="付款钱包" value={order.payment.payer ?? ''} /><Info label="固定收款钱包" value={order.payment.provider_wallet ?? ''} />
        <Info label="传输费" value={order.payment.delivery_fee} /><Info label="暂存预留费" value={order.payment.reservation_fee} /><Info label="时效优先费" value={order.payment.priority_fee} />
        <Info label={settlementAmountsLabel(order)} value={`${order.payment.earned} / ${order.payment.withdrawn} / ${order.payment.refunded}`} />
        {deadlineRows(order).map(([label, value]) => <Info key={label} label={label} value={value} />)}
        <p>时效优先费只有在其截止前完成整单才赚取。过了时效截止，仍可在结算硬截止前完成交付。暂存费支付容量预留，不代表持续存储证明。</p>
        {order.payment.tx_hash ? <Info label="原交易" value={order.payment.tx_hash} /> : null}
      </> : <p>本订单免费，不调用钱包或链上付款。</p>}
      {order.windows.length ? <ul className="grid gap-1">{order.windows.map(window => <li key={window.index}>窗口 {window.index + 1} · 密文已落盘 {window.committed_bytes} / {window.bytes + 68} 字节{window.decryption_proven ? ' · 已证明解密' : ''}</li>)}</ul> : null}
      {order.sha256 ? <Info label="原文件 SHA-256" value={order.sha256} /> : null}
    </Details>
  </section>
}

function Feedback({ error, notice }: { error: string; notice: string }) {
  if (error) return <InlineError error={error} />
  if (notice) return <p role="status" className="flex items-center gap-1.5 text-meta text-success"><CircleCheck aria-hidden="true" className="size-4 shrink-0" />{notice}</p>
  return null
}

/** Only the button that copies the 64-character ID, to send to whoever sends a file. */
function ConnectionId({ value }: { value: string }) {
  const [copied, setCopied] = useState(false)
  useEffect(() => {
    if (!copied) return
    const timer = setTimeout(() => setCopied(false), 2000)
    return () => clearTimeout(timer)
  }, [copied])
  return <div className="grid gap-1 text-meta">
    <span className="text-muted-foreground">{transferCopy.connectionId}</span>
    <span className="flex min-w-0 items-center gap-2">
      <Button type="button" variant="outline" size="sm" className="shrink-0" onClick={() => { void navigator.clipboard.writeText(value).then(() => setCopied(true), () => {}) }}>{copied ? <CircleCheck aria-hidden="true" /> : <Copy aria-hidden="true" />}{copied ? '已复制' : '复制'}</Button>
    </span>
  </div>
}

function Info({ label, value }: { label: string; value: ReactNode }) { return <div className="grid gap-1 text-meta sm:grid-cols-[10rem_minmax(0,1fr)]"><span className="text-muted-foreground">{label}</span><span className="break-all text-foreground">{value}</span></div> }
function Toggle({ label, icon: Icon, checked, disabled, change }: { label: string; icon?: LucideIcon; checked: boolean; disabled: boolean; change(value: boolean): void }) {
  return <label className="flex min-h-11 items-center justify-between gap-3">
    <span className="flex items-center gap-1.5">{label}{Icon ? <Icon aria-hidden="true" className="size-4 text-muted-foreground" /> : null}</span>
    <input type="checkbox" role="switch" className="switch" checked={checked} disabled={disabled} onChange={event => change(event.target.checked)} />
  </label>
}
