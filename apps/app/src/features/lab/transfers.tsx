import { selectionOwner } from '@vibeface/client/lib/selection.ts'
import { useClipboard } from '@/lib/use-clipboard'
import { FieldNote,FormField } from "@/components/arc/form-field"
import { BusyIcon,Details,EmptyState,InlineError,Segmented,useAction } from '@/components/arc/product-kit'
import { terms } from '@/lib/glossary'
import { webPlatform } from '@/lib/platform'
import { useDelivery } from '@vibeface/client/delivery/client.ts'
import { canRestart,deadlineRows,evidenceLabels,modeOptions,orderActions,orderHeading,orderStatus,orderSummary,orderTitle,providerChoices,publicationLabel,receiverChoices,receivingBatches,settlementAmountsLabel,sizeLabel,transferCopy,walletNotice } from '@vibeface/client/delivery/model.ts'
import { type DeliveryDraft,type DeliveryOrder,type DeliverySettings,type TransferFile } from '@vibeface/client/delivery/types.ts'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { messageOf } from '@vibeface/client/lib/node.ts'
import { Button } from '@vibeface/uiarc/button/button.tsx'
import { Input } from '@vibeface/uiarc/input/input.tsx'
import { Select } from "@vibeface/uiarc/select/select.tsx"
import { Switch } from "@vibeface/uiarc/switch/switch.tsx"
import { ArrowDownLeft,ArrowDownToLine,ArrowRightLeft,ArrowUpRight,CircleCheck,Copy,File,FlaskConical,RefreshCw,type LucideIcon } from 'lucide-react'
import { useEffect,useRef,useState,type ReactNode } from 'react'

const roleIcons = { sender: ArrowUpRight, receiver: ArrowDownLeft, provider: ArrowRightLeft } as const

export function Transfers() {
  const { ready } = useDevice()
  return ready ? <TransferPage key={`${ready.generation}:${ready.entry.device_id}`} connection={ready} /> : <EmptyState icon={ArrowRightLeft} title="设备暂时离线" description={transferCopy.offline} />
}

function TransferPage({ connection }: { connection: NonNullable<ReturnType<typeof useDevice>['ready']> }) {
  const delivery = useDelivery(connection, webPlatform)
  const { directory } = useDevice()
  const [draft, setDraft] = useState<DeliveryDraft>({ receiver_peer: '', provider_peer: '', mode: 'charity', acceptance: 'recoverable', max_service_amount: '0', max_gas_amount: '0', budget_id: null })
  const [file, setFile] = useState<TransferFile | null>(null)
  const [cleanupError, setCleanupError] = useState('')
  const selection = useRef<ReturnType<typeof selectionOwner<TransferFile>> | null>(null)
  useEffect(() => {
    let active = true
    const owner = selectionOwner<TransferFile>((error) => { if (active) setCleanupError(error.message); else console.error('选择副本清理失败，副本保留待清除。', error) })
    selection.current = owner
    return () => { active = false; owner.close() }
  }, [])
  // Feedback shows next to the control that caused it, not at the top of a long page.
  const [origin, setOrigin] = useState<'page' | 'form' | 'order'>('page')
  const picker = useAction()
  const status = delivery.status
  const selected = delivery.selected
  const availableBudgets = delivery.budgets.filter(budget => budget.state === 'open' && budget.payer.toLowerCase() === status?.wallet.address?.toLowerCase())
  const preparedBatch = delivery.orders.filter(order => order.role === 'sender' && order.mode === 'paid' && order.phase === 'prepared' && order.payment.budget_id === draft.budget_id)
  const canConfirmBatch = preparedBatch.length === 2 && preparedBatch.every(order => order.windows.length === 1)
  const working = delivery.busy !== null || picker.busy !== null
  const change = (patch: Partial<DeliverySettings>) => {
    setOrigin('page')
    if (status) void delivery.configure({ enabled: status.enabled, charity_enabled: status.charity_enabled, paid_enabled: status.paid_enabled, ...patch })
  }
  const feedback = (where: typeof origin) => origin === where ? <Feedback error={delivery.error} notice={delivery.notice} /> : null
  const mine = receiverChoices(directory?.members ?? [], status?.node_peer ?? '')
  const picked = mine.some(item => item.device_id === draft.receiver_peer)
  // Once a pasted ID shows its field, clearing it must not fold the field shut mid-edit.
  const other = useRef(false)
  if (!picked && draft.receiver_peer) other.current = true
  const receiverId = <FormField><Input label={transferCopy.receiver} id="transfer-receiver" className="font-mono" value={picked ? '' : draft.receiver_peer} autoCapitalize="none" autoCorrect="off" spellCheck={false} maxLength={64} required={!picked} disabled={working} onChange={event => setDraft({ ...draft, receiver_peer: event.target.value })} /></FormField>
  const providers = providerChoices(delivery.providers, draft.mode)
  const unusable = working || !status?.enabled || !file || !draft.receiver_peer || status.max_file_bytes === 0 || draft.mode === 'charity' && !status.charity_enabled || draft.mode === 'paid' && !status.paid_enabled
  const prepare = (restart?: string) => {
    if (!file || unusable) return
    setOrigin('form')
    const owner = selection.current!
    void owner.use(([selected]) => delivery.prepare(draft, selected, restart)).then(success => { if (success && owner.replace([])) setFile(null) }).catch((cause) => setCleanupError(String(cause)))
  }
  return <div className="mx-auto grid w-full max-w-(--page-width) gap-6">
    {feedback('page')}
    {!status ? <p role="status" className="text-sm text-secondary">{transferCopy.loading}</p> : <>
      <section aria-labelledby="transfer-settings" className="grid gap-4 rounded-panel border bg-surface p-4">
        <h2 id="transfer-settings" className="text-lg font-medium">传输设置</h2>
        <div className="grid">
          <Toggle label="启用文件传输" checked={status.enabled} disabled={!delivery.canManage || working} change={enabled => change({ enabled })} />
          {status.enabled ? <>
            <Toggle label={transferCopy.charity} checked={status.charity_enabled} disabled={!delivery.canManage || working} change={charity_enabled => change({ charity_enabled })} />
            <Toggle label={transferCopy.paid} icon={FlaskConical} checked={status.paid_enabled} disabled={!delivery.canManage || working} change={paid_enabled => change({ paid_enabled })} />
          </> : null}
        </div>
        {status.enabled ? <ConnectionId value={status.node_peer} /> : <p className="text-sm text-secondary">{transferCopy.off}</p>}
        {!delivery.canManage ? <p className="flex items-center gap-1.5 text-sm text-secondary"><terms.admin.icon aria-hidden="true" className="size-4 shrink-0" />{transferCopy.adminOnly}</p> : null}
        {status.paid_enabled || status.wallet.state !== 'absent' ? <p className="text-sm text-secondary">{walletNotice(status.wallet)}</p> : null}
        {status.wallet.state === 'unavailable' ? <p role="alert" className="text-sm text-warning">{status.wallet.reason}</p> : null}
        <Details>
          {status.wallet.address ? <Info label="设备钱包" value={status.wallet.address} /> : null}
          {status.receiver_key ? <><Info label="当前接收密钥 ID" value={status.receiver_key.key_id} /><Info label="当前接收密钥序号" value={status.receiver_key.serial.toString()} /></> : null}
          <p>{transferCopy.walletDetails}</p>
        </Details>
      </section>
      {status.enabled && delivery.canManage ? <form className="grid gap-4 rounded-panel border bg-surface p-4" onSubmit={event => { event.preventDefault(); prepare() }}>
        <h2 className="text-lg font-medium">发送文件</h2>
        {mine.length ? <>
          <FormField>
            <Select label={transferCopy.sendTo} id="transfer-to" className="w-full" disabled={working} value={"arc:" + (picked ? draft.receiver_peer : '')} onValueChange={selected => { const value = selected.slice(4);  setDraft({ ...draft, receiver_peer: value })  }} options={[{ value: "arc:", label: transferCopy.pickDevice, disabled: true }, ...mine.map(item => ({ value: "arc:" + item.device_id, label: item.name }))]} />
          </FormField>
          <Details label={transferCopy.otherDevice} open={other.current || undefined}>{receiverId}</Details>
        </> : receiverId}
        <FormField><Input label={transferCopy.provider} id="transfer-provider" className="font-mono" value={draft.provider_peer} autoCapitalize="none" autoCorrect="off" spellCheck={false} maxLength={64} required disabled={working} onChange={event => setDraft({ ...draft, provider_peer: event.target.value })} />
          {providers.length ? <ul aria-label={transferCopy.providers} className="flex flex-wrap gap-2">{providers.map(item => <li key={item.device_id}>
            <Button type="button" size="sm" variant={draft.provider_peer === item.device_id ? 'secondary' : "secondary"} aria-pressed={draft.provider_peer === item.device_id} title={item.device_id} disabled={working} onClick={() => setDraft({ ...draft, provider_peer: item.device_id })}>
              <ArrowRightLeft aria-hidden="true" />{directory?.members.find(member => member.device_id === item.device_id)?.name ?? <span className="font-mono">{item.device_id.slice(0, 8)}…</span>}
              {draft.mode === 'paid' && item.offer ? <span className="text-secondary tabular-nums">{item.offer.price}</span> : null}
            </Button>
          </li>)}</ul> : null}
          <FieldNote>{transferCopy.providerHint}</FieldNote></FormField>
        <Segmented label="传输方式" value={draft.mode} options={modeOptions} labels disabled={working} onChange={mode => setDraft({ ...draft, mode })} />
        {draft.mode === 'paid' ? <>
          <p className="text-sm text-secondary">{transferCopy.protected}</p>
          <FormField>
            <Select required label="预付预算" placeholder="选择发送前已建立的预算" id="transfer-budget" className="w-full" disabled={working} value={draft.budget_id ? "arc:" + draft.budget_id : ""} onValueChange={selected => { const value = selected.slice(4);
              const budget = availableBudgets.find(item => item.budget_id === value)
              if (budget) setDraft({ ...draft, budget_id: budget.budget_id, provider_peer: budget.provider_peer, max_gas_amount: budget.gas_cap })
             }} options={availableBudgets.map(budget => ({ value: "arc:" + budget.budget_id, label: `${budget.budget_id.slice(0, 12)} · 可分配 ${budget.free}` }))} />
            <FieldNote>预算固定收款钱包和中转设备。先在发送设备本机建立预付预算，再刷新此页。</FieldNote>
          </FormField>
          <FormField><Input label={transferCopy.serviceCap} id="transfer-service-cap" inputMode="numeric" pattern="[0-9]+" value={draft.max_service_amount} required disabled={working} onChange={event => setDraft({ ...draft, max_service_amount: event.target.value })} /></FormField>
          <FormField><Input label={transferCopy.gasCap} id="transfer-gas-cap" value={draft.max_gas_amount} readOnly /></FormField>
        </> : <p className="flex items-center gap-1.5 text-sm text-secondary"><terms.encrypted.icon aria-hidden="true" className="size-4 shrink-0 text-success" />{transferCopy.free}</p>}
        <div className="flex flex-wrap items-center gap-x-3 gap-y-2">
          <Button type="button" variant="secondary" disabled={working} onClick={() => void picker.run('pick', async () => { const owner = selection.current!; const picked = await webPlatform.files?.pick(); if (picked && owner.replace([picked])) setFile(picked) })}><BusyIcon busy={picker.busy === 'pick'} icon={File} />选择文件</Button>
          <span className="min-w-0 break-all text-sm text-secondary">{file ? <><span className="text-foreground">{file.name}</span> · {sizeLabel(file.size)}</> : transferCopy.maxSize(status.max_file_bytes)}</span>
        </div>
        {picker.error || cleanupError ? <p role="alert" className="text-sm text-danger">{messageOf(picker.error || cleanupError)}</p> : null}
        {file && file.size > status.max_file_bytes ? <p role="alert" className="text-sm text-danger">{transferCopy.tooLarge(status.max_file_bytes)}</p> : null}
        {feedback('form')}
        <div className="flex flex-wrap gap-2">
          <Button type="submit" disabled={unusable}><BusyIcon busy={delivery.busy === 'prepare'} />{transferCopy.prepare(draft.mode)}</Button>
          {selected && canRestart(selected) ? <Button type="button" variant="secondary" disabled={unusable} onClick={() => { prepare(selected.intent_id) }}>作为新订单再次发送</Button> : null}
        </div>
        <Details><p>{transferCopy.prepareDetails}</p>{draft.mode === 'paid' ? <p>{transferCopy.units}</p> : null}</Details>
      </form> : null}
    </>}
    {delivery.budgets.length ? <section aria-label="预付预算" className="grid gap-3 rounded-panel border bg-surface p-4">
      <h2 className="text-lg font-medium">预付预算</h2>
      {delivery.budgets.map(budget => <div key={budget.budget_id} className="grid gap-2">
        <Info label="预算" value={budget.budget_id} />
        <Info label="可分配 / 已占用" value={`${budget.free} / ${budget.locked}`} />
        <Info label="已收到待结算的文件片段" value={budget.ready_windows.length.toString()} />
        <Info label="累计赚取 / 提款 / 退款" value={`${budget.earned} / ${budget.withdrawn} / ${budget.refunded}`} />
        {budget.payee.toLowerCase() === status?.wallet.address?.toLowerCase() ? <Button type="button" variant="secondary" disabled={working || budget.ready_windows.length === 0 || !delivery.canManage || !status?.enabled || !status.paid_enabled || status.wallet.state !== 'unlocked'} onClick={() => { setOrigin('page'); void delivery.flushBudget(budget.budget_id) }}>结算当前已就绪批次（最多两窗）</Button> : null}
      </div>)}
      {canConfirmBatch ? <div className="grid gap-2">
        {preparedBatch.map(order => <p key={order.order_id} className="text-sm">{order.name} · 服务费 {order.payment.total}</p>)}
        <Button type="button" disabled={working || !delivery.canManage || !status?.enabled || !status.paid_enabled || status.wallet.state !== 'unlocked'} onClick={() => { setOrigin('page'); void delivery.confirmBatch(preparedBatch) }}>确认这两笔并同批分配预算</Button>
      </div> : null}
    </section> : null}
    <section aria-labelledby="transfer-orders" className="grid gap-3">
      <div className="flex items-center justify-between gap-3">
        <h2 id="transfer-orders" className="text-lg font-medium">传输记录</h2>
        <Button type="button" variant="ghost" size="sm" disabled={working} onClick={() => { setOrigin('page'); void delivery.refresh() }}><BusyIcon busy={delivery.busy === 'refresh'} icon={RefreshCw} />刷新</Button>
      </div>
      {receivingBatches(delivery.orders).map(([budgetId, batch]) => <div key={budgetId} className="grid gap-2 rounded-panel border bg-surface p-4">
        {batch.map(order => <p key={order.order_id} className="text-sm">{orderTitle(order)}</p>)}
        <Button type="button" disabled={working || !delivery.canManage || !status?.enabled || !status.paid_enabled} onClick={() => { setOrigin('page'); void delivery.acceptBatch(batch) }}>同时接收这两份文件</Button>
      </div>)}
      {delivery.orders.length ? <ul className="divide-y overflow-hidden rounded-panel border bg-surface">{delivery.orders.map(order => {
        const Icon = roleIcons[order.role]
        return <li key={order.order_id}><Button variant="ghost" type="button" className="interactive-row flex w-full items-center gap-3 text-left" aria-pressed={delivery.selected?.order_id === order.order_id} onClick={() => delivery.select(order.order_id)}>
          <Icon aria-hidden="true" className="size-4 shrink-0 text-secondary" />
          <span className="grid min-w-0 gap-0.5">
            <span className="truncate font-medium">{orderTitle(order)}</span>
            <span className="text-sm text-secondary">{orderSummary(order)}</span>
          </span>
        </Button></li>
      })}</ul> : <EmptyState icon={ArrowRightLeft} title={transferCopy.noRecords} />}
    </section>
    {selected ? <OrderDetails key={selected.order_id} order={selected} busy={working} canManage={delivery.canManage} enabled={!!status?.enabled && (selected.mode === 'paid' ? status.paid_enabled : status.charity_enabled)} walletReady={status?.wallet.state === 'unlocked'} feedback={feedback('order')}
      confirm={() => { setOrigin('order'); void delivery.confirm(selected) }} accept={() => { setOrigin('order'); void delivery.accept(selected) }}
      reconcile={() => { setOrigin('order'); void delivery.reconcile(selected) }} save={() => { setOrigin('order'); void delivery.save(selected) }} /> : null}
  </div>
}

function OrderDetails({ order, busy, canManage, enabled, walletReady, feedback, confirm, accept, reconcile, save }: { order: DeliveryOrder; busy: boolean; canManage: boolean; enabled: boolean; walletReady: boolean; feedback: ReactNode; confirm(): void; accept(): void; reconcile(): void; save(): void }) {
  const heading = useRef<HTMLHeadingElement>(null)
  useEffect(() => { heading.current?.focus() }, [])
  const paid = order.mode === 'paid'
  const can = orderActions(order)
  return <section className="grid gap-4 rounded-panel border bg-surface p-4" aria-labelledby="transfer-detail">
    <h2 ref={heading} id="transfer-detail" tabIndex={-1} className="text-lg font-medium outline-none">{orderHeading(order)}</h2>
    <p>{orderStatus(order)}</p>
    {order.name ? <Info label="文件" value={`${order.name} · ${sizeLabel(order.bytes)}`} /> : null}
    <Info label="传输方式" value={paid ? transferCopy.paid : <span className="inline-flex items-center gap-1.5"><terms.encrypted.icon aria-hidden="true" className="size-4 text-success" />{transferCopy.charity}</span>} />
    {paid ? <>
      <Info label="预付预算" value={order.payment.budget_id} />
      <Info label={transferCopy.serviceTotal} value={order.payment.total} />
      <Info label={transferCopy.gasCap} value={order.payment.gas_cap} />
    </> : null}
    {order.error ? <p role="alert" className="text-sm text-danger">{order.error}</p> : null}
    {feedback}
    <div className="flex flex-wrap gap-2">
      {can.confirm ? <Button type="button" disabled={busy || !canManage || !enabled || paid && !walletReady} onClick={confirm}>{transferCopy.confirm(order.mode)}</Button> : null}
      {can.accept ? <Button type="button" disabled={busy || !canManage || !enabled} onClick={accept}>接收文件</Button> : null}
      {can.save ? <Button type="button" variant={order.role === 'receiver' ? "primary" : "secondary"} disabled={busy} onClick={save}><ArrowDownToLine aria-hidden="true" />保存文件</Button> : null}
      <Button type="button" variant="secondary" disabled={busy || !canManage || !enabled} onClick={reconcile}>检查进度</Button>
    </div>
    {!enabled ? <p className="text-sm text-secondary">文件传输或此传输方式已关闭。开启后可继续原订单，已收到的文件仍可保存。</p> : null}
    {paid && order.phase === 'prepared' && !walletReady ? <p className="text-sm text-warning">{transferCopy.walletFirst}</p> : null}
    {order.payment.state === 'unknown' || order.phase === 'reconciling' ? <p className="text-sm text-warning">{transferCopy.unknownPayment}</p> : null}
    <Details>
      <p>{evidenceLabels[order.evidence]}。交付确认不表示真人已阅读。</p>
      <Info label="订单" value={order.order_id} /><Info label="付款意图" value={order.intent_id} />
      <Info label="发送设备" value={order.source_peer} /><Info label="接收设备" value={order.receiver_peer} /><Info label="中转设备" value={order.provider_peer} />
      <Info label="验收条件" value={paid ? '完整密文持有证明与正确密钥包可取后结算' : '公益收据'} />
      {paid ? <>
        <Info label="测试链 ID" value={order.payment.chain_id ?? ''} /><Info label="测试资产合约" value={order.payment.asset ?? ''} /><Info label="付款钱包" value={order.payment.payer ?? ''} /><Info label="固定收款钱包" value={order.payment.provider_wallet ?? ''} />
        <Info label="传输费" value={order.payment.delivery_fee} /><Info label="暂存预留费" value={order.payment.reservation_fee} /><Info label="时效优先费" value={order.payment.priority_fee} />
        <Info label={settlementAmountsLabel(order, 'order')} value={`${order.payment.earned} / ${order.payment.refunded}`} />
        {order.payment.settlement ? <>
          <Info label={settlementAmountsLabel(order, 'budget')} value={`${order.payment.settlement.earned} / ${order.payment.settlement.withdrawn} / ${order.payment.settlement.refunded}`} />
          <Info label="批次状态" value={publicationLabel(order)} />
          {order.payment.settlement.publication ? <>
            <Info label="本批目标赚取金额" value={order.payment.settlement.publication.total_amount} />
            <Info label="本批完整金额截止" value={new Date(order.payment.settlement.publication.economic_cutoff * 1000).toLocaleString()} />
            {order.payment.settlement.publication.availability_risk ? <p role="status" className="text-sm text-warning">{order.payment.settlement.publication.availability_risk}</p> : null}
          </> : null}
        </> : null}
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
  if (notice) return <p role="status" className="flex items-center gap-1.5 text-sm text-success"><CircleCheck aria-hidden="true" className="size-4 shrink-0" />{notice}</p>
  return null
}

/** Only the button that copies the 64-character ID, to send to whoever sends a file. */
function ConnectionId({ value }: { value: string }) {
  const { copied, copy } = useClipboard(value)
  return <div className="grid gap-1 text-sm">
    <span className="text-secondary">{transferCopy.connectionId}</span>
    <span className="flex min-w-0 items-center gap-2">
      <Button type="button" variant="secondary" size="sm" className="shrink-0" onClick={() => { void copy(value) }}>{copied ? <CircleCheck aria-hidden="true" /> : <Copy aria-hidden="true" />}{copied ? '已复制' : '复制'}</Button>
    </span>
  </div>
}

function Info({ label, value }: { label: string; value: ReactNode }) { return <div className="grid gap-1 text-sm sm:grid-cols-[10rem_minmax(0,1fr)]"><span className="text-secondary">{label}</span><span className="break-all text-foreground">{value}</span></div> }
function Toggle({ label, icon: Icon, checked, disabled, change }: { label: string; icon?: LucideIcon; checked: boolean; disabled: boolean; change(value: boolean): void }) {
  return <label className="flex min-h-11 items-center justify-between gap-3">
    <span className="flex items-center gap-1.5">{label}{Icon ? <Icon aria-hidden="true" className="size-4 text-secondary" /> : null}</span>
    <Switch checked={checked} disabled={disabled} onCheckedChange={next => { const checked = next === true;  change(checked)  }} />
  </label>
}
