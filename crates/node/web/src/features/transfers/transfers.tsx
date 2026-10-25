import { useEffect, useRef, useState, type ReactNode } from 'react'
import { ArrowDownToLine, ArrowRightLeft, File, RefreshCw } from 'lucide-react'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { useDelivery } from '@vibeface/client/delivery/client.ts'
import { canRead, canRestart, changeMode, deadlineRows, evidenceLabels, paymentLabels, phaseLabels, settlementAmountsLabel, walletNotice } from '@vibeface/client/delivery/model.ts'
import type { Acceptance, DeliveryDraft, DeliveryMode, DeliveryOrder, DeliverySettings, TransferFile } from '@vibeface/client/delivery/types.ts'
import { messageOf } from '@vibeface/client/lib/node.ts'
import { BusyIcon, EmptyState, Segmented, useAction } from '@/components/app/kit'
import { Button } from '@/components/ui/button'
import { Field, FieldDescription, FieldLabel } from '@/components/ui/field'
import { Input } from '@/components/ui/input'
import { webPlatform } from '@/lib/platform'

const modes: Array<{ value: DeliveryMode; label: string }> = [{ value: 'charity', label: '公益转发' }, { value: 'paid', label: '付费交付' }]
const acceptances: Array<{ value: Acceptance; label: string }> = [{ value: 'recoverable', label: '确认可恢复后结算' }, { value: 'strict', label: '证明实际解密后结算' }]

export function Transfers() {
  const { ready } = useDevice()
  return ready ? <TransferPage key={`${ready.generation}:${ready.entry.device_id}`} connection={ready} /> : <EmptyState icon={ArrowRightLeft} title="设备暂时离线" description="重新连接后可查询原订单，不会自动重新付款。" />
}

function TransferPage({ connection }: { connection: NonNullable<ReturnType<typeof useDevice>['ready']> }) {
  const delivery = useDelivery(connection, webPlatform)
  const [draft, setDraft] = useState<DeliveryDraft>({ receiver_peer: '', provider_peer: '', mode: 'charity', acceptance: 'recoverable', max_service_amount: '0', max_gas_amount: '0' })
  const [file, setFile] = useState<TransferFile | null>(null)
  const picker = useAction()
  const status = delivery.status
  const selected = delivery.selected
  const working = delivery.busy !== null || picker.busy !== null
  const change = (patch: Partial<DeliverySettings>) => {
    if (status) void delivery.configure({ enabled: status.enabled, charity_enabled: status.charity_enabled, paid_enabled: status.paid_enabled, ...patch })
  }
  return <div className="mx-auto grid w-full max-w-(--page-width) gap-8">
    <div className="flex flex-wrap items-start justify-between gap-3">
      <p className="max-w-lg text-meta text-muted-foreground">查找设备和公益转发免费，付费交付仅供本地测试。</p>
      <Button type="button" variant="outline" size="sm" disabled={working} onClick={() => void delivery.refresh()}><BusyIcon busy={delivery.busy === 'refresh'} icon={RefreshCw} />刷新</Button>
    </div>
    {delivery.error ? <p role="alert" className="text-meta text-danger">{delivery.error}</p> : null}
    {delivery.notice ? <p role="status" className="text-meta text-success">{delivery.notice}</p> : null}
    {!status ? <p role="status" className="text-meta text-muted-foreground">正在读取传输状态。</p> : <>
      <section aria-labelledby="transfer-settings" className="grid gap-4 rounded-xl border bg-card p-4">
        <h2 id="transfer-settings" className="text-heading font-semibold">此设备的传输设置</h2>
        <Info label="发送设备" value={status.node_peer} />
        <div className="flex flex-wrap gap-x-6 gap-y-3">
          <Toggle label="启用文件传输" checked={status.enabled} disabled={!delivery.canManage || working} change={enabled => change({ enabled })} />
          <Toggle label="公益转发" checked={status.charity_enabled} disabled={!delivery.canManage || working} change={charity_enabled => change({ charity_enabled })} />
          <Toggle label="付费交付（本地测试）" checked={status.paid_enabled} disabled={!delivery.canManage || working} change={paid_enabled => change({ paid_enabled })} />
        </div>
        <p className="text-meta text-muted-foreground">{walletNotice(status.wallet)}{!delivery.canManage ? ' 只有已加入的管理员能更改设置或确认订单。' : ''}</p>
        {status.wallet.state === 'unavailable' ? <p role="alert" className="text-meta text-warning">{status.wallet.reason}</p> : null}
        {status.wallet.address ? <Info label="设备钱包" value={status.wallet.address} /> : null}
        <p className="text-meta text-muted-foreground">钱包的创建、解锁和备份通过本机命令行完成。个人恢复文件不包含钱包和订单解密密钥。</p>
      </section>
      {status.enabled && delivery.canManage ? <form className="grid gap-4 rounded-xl border bg-card p-4" onSubmit={event => { event.preventDefault(); if (file) void delivery.prepare(draft, file) }}>
        <h2 className="text-heading font-semibold">发送文件</h2>
        <Field><FieldLabel htmlFor="transfer-receiver">接收设备的连接 ID</FieldLabel><Input id="transfer-receiver" className="font-mono" value={draft.receiver_peer} autoCapitalize="none" autoCorrect="off" spellCheck={false} maxLength={64} required disabled={working} onChange={event => setDraft({ ...draft, receiver_peer: event.target.value })} /></Field>
        <Field><FieldLabel htmlFor="transfer-provider">服务设备的连接 ID</FieldLabel><Input id="transfer-provider" className="font-mono" value={draft.provider_peer} autoCapitalize="none" autoCorrect="off" spellCheck={false} maxLength={64} required disabled={working} onChange={event => setDraft({ ...draft, provider_peer: event.target.value })} /><FieldDescription>付款不会让服务设备加入你的设备列表。</FieldDescription></Field>
        <Segmented label="传输方式" value={draft.mode} options={modes} labels disabled={working} onChange={mode => setDraft(changeMode(draft, mode))} />
        {draft.mode === 'paid' ? <>
          <Segmented label="结算条件" value={draft.acceptance} options={acceptances} labels disabled={working} onChange={acceptance => setDraft({ ...draft, acceptance })} />
          <Field><FieldLabel htmlFor="transfer-service-cap">服务费上限（测试代币最小单位）</FieldLabel><Input id="transfer-service-cap" inputMode="numeric" pattern="[0-9]+" value={draft.max_service_amount} required disabled={working} onChange={event => setDraft({ ...draft, max_service_amount: event.target.value })} /></Field>
          <Field><FieldLabel htmlFor="transfer-gas-cap">链费上限（测试链原生币最小单位）</FieldLabel><Input id="transfer-gas-cap" inputMode="numeric" pattern="[0-9]+" value={draft.max_gas_amount} required disabled={working} onChange={event => setDraft({ ...draft, max_gas_amount: event.target.value })} /></Field>
        </> : <p className="text-meta text-muted-foreground">文件全程加密，公益收据仅确认接收，不产生链上证明或付款。</p>}
        <div className="grid justify-items-start gap-2">
          <Button type="button" variant="outline" disabled={working} onClick={() => void picker.run('pick', async () => { const picked = await webPlatform.files?.pick(); if (picked) setFile(picked) })}><BusyIcon busy={picker.busy === 'pick'} icon={File} />选择文件</Button>
          <p className="text-meta text-muted-foreground">{file ? `${file.name} · ${file.size} 字节` : '尚未选择文件'}。当前最多 {status.max_file_bytes} 字节。</p>
          {picker.error ? <p role="alert" className="text-meta text-danger">{messageOf(picker.error)}</p> : null}
        </div>
        <Button type="submit" className="justify-self-start" disabled={working || !file || status.max_file_bytes === 0 || draft.mode === 'charity' && !status.charity_enabled || draft.mode === 'paid' && !status.paid_enabled}><BusyIcon busy={delivery.busy === 'prepare'} />准备订单并查看费用</Button>
        {selected && canRestart(selected) ? <Button type="button" variant="outline" className="justify-self-start" disabled={working || !file || status.max_file_bytes === 0 || draft.mode === 'charity' && !status.charity_enabled || draft.mode === 'paid' && !status.paid_enabled} onClick={() => { if (file) void delivery.prepare(draft, file, selected.intent_id) }}>作为新订单再次发送</Button> : null}
        <p className="text-meta text-muted-foreground">准备订单会把文件交给当前连接的可信设备加密，尚不付款。同一文件和条款复用原意图。原订单确认结束后，选择相同文件和条款，点击“作为新订单再次发送”才能创建另一笔订单。</p>
      </form> : !status.enabled ? <p className="text-meta text-muted-foreground">文件传输默认关闭。关闭时仍可免费查找设备，并查询已有订单。</p> : null}
    </>}
    <section aria-labelledby="transfer-orders" className="grid gap-3">
      <h2 id="transfer-orders" className="text-heading font-semibold">订单</h2>
      {delivery.orders.length ? <ul className="divide-y overflow-hidden rounded-xl border bg-card">{delivery.orders.map(order => <li key={order.order_id}><button type="button" className="interactive-row grid w-full gap-1 px-4 py-3 text-left" aria-pressed={delivery.selected?.order_id === order.order_id} onClick={() => delivery.select(order.order_id)}>
        <span className="break-all font-medium">{order.name ?? `密文订单 ${order.order_id.slice(0, 12)}`}</span>
        <span className="text-meta text-muted-foreground">{order.mode === 'charity' ? '公益' : '付费'} · {order.bytes} 字节 · {phaseLabels[order.phase]}</span>
        <span className="text-meta text-muted-foreground">{evidenceLabels[order.evidence]} · {paymentLabels[order.payment.state]}</span>
      </button></li>)}</ul> : <p className="text-meta text-muted-foreground">还没有传输订单。</p>}
    </section>
    {selected ? <OrderDetails key={selected.order_id} order={selected} busy={working} canManage={delivery.canManage} walletReady={status?.wallet.state === 'unlocked'} confirm={() => void delivery.confirm(selected)} accept={() => void delivery.accept(selected)} reconcile={() => void delivery.reconcile(selected)} save={() => void delivery.save(selected)} /> : null}
  </div>
}

function OrderDetails({ order, busy, canManage, walletReady, confirm, accept, reconcile, save }: { order: DeliveryOrder; busy: boolean; canManage: boolean; walletReady: boolean; confirm(): void; accept(): void; reconcile(): void; save(): void }) {
  const heading = useRef<HTMLHeadingElement>(null)
  useEffect(() => { heading.current?.focus() }, [])
  const paid = order.mode === 'paid'
  return <section className="grid gap-4 rounded-xl border bg-card p-4" aria-labelledby="transfer-detail">
    <h2 ref={heading} id="transfer-detail" tabIndex={-1} className="text-heading font-semibold outline-none">{order.phase === 'prepared' ? '确认这笔订单' : '订单详情'}</h2>
    <p>{phaseLabels[order.phase]} · {paymentLabels[order.payment.state]}</p>
    <p className="text-meta text-muted-foreground">{evidenceLabels[order.evidence]}。交付确认不表示真人已阅读。</p>
    <Info label="订单" value={order.order_id} /><Info label="付款意图" value={order.intent_id} />
    {order.name ? <Info label="文件" value={`${order.name} · ${order.bytes} 字节`} /> : null}
    <Info label="发送设备" value={order.source_peer} /><Info label="接收设备" value={order.receiver_peer} /><Info label="服务设备" value={order.provider_peer} />
    <Info label="验收条件" value={paid ? order.acceptance === 'strict' ? '接收设备 证明实际解密后结算' : '完整密文持有证明与正确密钥包可取后结算' : '公益收据'} />
    {paid ? <>
      <Info label="测试链 ID" value={order.payment.chain_id ?? ''} /><Info label="测试资产合约" value={order.payment.asset ?? ''} /><Info label="付款钱包" value={order.payment.payer ?? ''} /><Info label="固定收款钱包" value={order.payment.provider_wallet ?? ''} />
      <div className="grid gap-2 border-y py-3 text-meta">
        <Info label="传输费" value={order.payment.delivery_fee} /><Info label="暂存预留费" value={order.payment.reservation_fee} /><Info label="时效优先费" value={order.payment.priority_fee} /><Info label="服务费合计（最小单位）" value={order.payment.total} /><Info label="链费上限（原生币最小单位）" value={order.payment.gas_cap} />
        <Info label={settlementAmountsLabel(order)} value={`${order.payment.earned} / ${order.payment.withdrawn} / ${order.payment.refunded}`} />
      </div>
      {deadlineRows(order).map(([label, value]) => <Info key={label} label={label} value={value} />)}
      <p className="text-meta text-muted-foreground">时效优先费只有在其截止前完成整单才赚取。过了时效截止，仍可在结算硬截止前完成交付。暂存费支付容量预留，不代表持续存储证明。</p>
      {order.payment.tx_hash ? <Info label="原交易" value={order.payment.tx_hash} /> : null}
      {order.acceptance === 'strict' ? <p className="text-meta text-warning">严格验收需要接收设备 提交解密证明。对方离线或未提交时，服务商可能收不到费用。</p> : null}
    </> : <p className="text-meta text-muted-foreground">本订单免费，不调用钱包或链上付款。</p>}
    {order.windows.length ? <ul className="grid gap-1 text-meta text-muted-foreground">{order.windows.map(window => <li key={window.index}>窗口 {window.index + 1} · 密文已落盘 {window.committed_bytes} / {window.bytes + 68} 字节{window.decryption_proven ? ' · 已证明解密' : ''}</li>)}</ul> : null}
    {order.sha256 ? <Info label="原文件 SHA-256" value={order.sha256} /> : null}
    {order.error ? <p role="alert" className="text-meta text-danger">{order.error}</p> : null}
    <div className="flex flex-wrap gap-2">
      {order.role === 'sender' && order.phase === 'prepared' ? <Button type="button" disabled={busy || !canManage || paid && !walletReady} onClick={confirm}>{paid ? '确认费用并提交原订单' : '确认公益传输'}</Button> : null}
      {order.role === 'receiver' && !canRead(order) && order.phase !== 'refunded' && order.phase !== 'failed' ? <Button type="button" disabled={busy || !canManage} onClick={accept}>接受此订单</Button> : null}
      <Button type="button" variant="outline" disabled={busy || !canManage} onClick={reconcile}>继续与对账</Button>
      {canRead(order) ? <Button type="button" variant="outline" disabled={busy} onClick={save}><ArrowDownToLine aria-hidden="true" />取回文件</Button> : null}
    </div>
    {paid && order.phase === 'prepared' && !walletReady ? <p className="text-meta text-warning">请先在发送设备 本机创建或解锁钱包，再刷新此订单。</p> : null}
    {order.payment.state === 'unknown' || order.phase === 'reconciling' ? <p className="text-meta text-warning">原交易结果尚未确认。继续与对账会查询原意图，不会创建另一笔付款。</p> : null}
  </section>
}

function Info({ label, value }: { label: string; value: ReactNode }) { return <div className="grid gap-1 text-meta sm:grid-cols-[10rem_minmax(0,1fr)]"><span className="text-muted-foreground">{label}</span><span className="break-all">{value}</span></div> }
function Toggle({ label, checked, disabled, change }: { label: string; checked: boolean; disabled: boolean; change(value: boolean): void }) {
  return <label className="flex min-h-11 items-center gap-2 text-meta"><input type="checkbox" role="switch" checked={checked} disabled={disabled} onChange={event => change(event.target.checked)} />{label}</label>
}
