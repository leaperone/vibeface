import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { createHash } from 'node:crypto';
import { resolve } from 'node:path';
import { test } from '@e2e-dev/web';
import { expect, secrets } from 'e2e';
import { acknowledgeInspection, deadline, event, facts, insideArtifacts, nextInspection, ordinaryDeadline, ordinaryReady, pairing, preserveScreenshot, request, root, runUi, saveEvidence } from './paid-delivery-control';
import { videoEvidence, webRecording } from './paid-recordings';

test('quotes and pays one recoverable file through the real Web UI while Android recovers offline',
  { platforms: ['web'], timeout: deadline() }, async ({ browser, screen, app }) => runUi('web', async () => {
    const environment = await facts();
    await browser.goto(`${environment.nodes.web.url}/join`);
    await screen.getByLabel('加入链接', { exact: true }).fill(secrets.get('paidWebInvitation'), { timeout: 30000 });
    await screen.getByLabel('这个浏览器叫什么？', { exact: true }).fill('VDP paid web sender');
    await screen.getByRole('button', '加入', { exact: true }).click();
    await expect.poll(async () => (await pairing('web')) !== null, { timeout: 45000, message: 'The real Web member reveals its pairing nonce' }).toBe(true);
    const joined = await pairing('web');
    assert(joined);
    await expect(screen.getByLabel(`安全码 ${joined.code.split('').join(' ')}`, { exact: true })).toBeVisible();
    await request('/approve', { role: 'web', peer: joined.peer, code: joined.code });
    await browser.goto(`${environment.nodes.web.url}/settings`);
    await screen.getByRole('switch', '实验功能', { exact: true }).check({ timeout: 30000 });
    await browser.goto(`${environment.nodes.web.url}/transfers`);
    await expect(screen.getByRole('heading', '传输设置', { exact: true })).toBeVisible({ timeout: 30000 });
    await expect(screen.getByRole('switch', '付费传输')).toBeChecked();
    await event('observer-ready', { role: 'web' });
    for (const kind of ['source-partial', 'source-unknown'] as const) {
      const inspection = await nextInspection(kind);
      assert(inspection.role === 'web' && inspection.nodePeer === environment.nodes.web.peer && inspection.orderId && inspection.intentId);
      await browser.reload();
      await expect(screen.getByRole('heading', '传输设置', { exact: true })).toBeVisible({ timeout: 30000 });
      const orders = browser.locator('button[aria-pressed]');
      await expect.poll(() => orders.count(), { timeout: 30000, message: 'The real original source has exactly one strict order' }).toBe(1);
      await orders.click();
      const actual = screen.getByRole('region', '传输详情', { exact: true });
      await actual.getByText('详细信息', { exact: true }).click();
      await expect(actual.getByText(inspection.orderId, { exact: true })).toBeVisible();
      await expect(actual.getByText(inspection.intentId, { exact: true })).toBeVisible();
      await expect(actual.getByText('12 / 12 / 17', { exact: true })).toBeVisible();
      await expect(actual.getByText('还没送达。交付确认不表示真人已阅读。', { exact: true })).toBeVisible();
      if (kind === 'source-partial') {
        await expect(actual.getByText('部分完成 · 服务费已提取', { exact: true })).toBeVisible();
        await expect(actual.getByText('已赚取 / 已提款 / 已退款', { exact: true })).toBeVisible();
        await expect(actual.getByText(/^窗口 1 .*已证明解密$/)).toBeVisible();
      } else {
        await expect(actual.getByText('正在确认付款结果 · 付款结果未知', { exact: true })).toBeVisible();
        await expect(actual.getByText('上次记录（赚取 / 提款 / 退款，到账待确认）', { exact: true })).toBeVisible();
        await expect(actual.getByText('付款结果还没确定。点“检查进度”会查询原来那笔，不会重复付款。', { exact: true })).toBeVisible();
        expect(await actual.getByText(/已证明解密/).count()).toBe(0);
      }
      await browser.setViewport({ width: 1440, height: 2200 });
      await actual.scrollIntoView();
      await acknowledgeInspection(inspection, await app.screenshot(kind));
      await browser.setViewport({ width: 1440, height: 1000 });
    }
    await ordinaryReady();
    await browser.reload();
    await expect(screen.getByRole('heading', '传输设置', { exact: true })).toBeVisible({ timeout: 30000 });
    await expect.poll(async () => (await facts()).paired.android, { timeout: 180000, message: 'The Android receiver joins through its actual invitation' }).toBe(true);
    await screen.getByLabel('接收方的连接 ID').fill(environment.nodes.android.peer);
    await screen.getByLabel('中转设备的连接 ID').fill(environment.nodes.provider.peer);
    await screen.getByRole('radio', '付费传输', { exact: true }).click();
    await expect(screen.getByRole('radio', '对方能取回后付款')).toBeChecked();
    await screen.getByLabel('服务费上限').fill(environment.serviceCap);
    await screen.getByLabel('链费上限').fill(environment.gasCap);
    const input = insideArtifacts(resolve(root, environment.sourceFile));
    const bytes = await readFile(input);
    assert.equal(bytes.length, 4096);
    assert.equal(createHash('sha256').update(bytes).digest('hex'), environment.sha256);
    await browser.evaluate(() => {
      const intercept = (click: MouseEvent) => {
        if (click.target instanceof HTMLInputElement && click.target.type === 'file' && click.target.parentElement === document.body) {
          click.preventDefault();
          document.removeEventListener('click', intercept, true);
        }
      };
      document.addEventListener('click', intercept, true);
      return null;
    });
    await screen.getByRole('button', '选择文件').click();
    await browser.locator('body > input[type="file"]').setInputFiles(input);
    await expect(screen.getByText(environment.name, { exact: true })).toBeVisible();
    await screen.getByRole('button', '查看费用').click();
    const details = screen.getByRole('region', '确认发送', { exact: true });
    await expect(details).toBeVisible({ timeout: 45000 });
    await details.getByText('详细信息', { exact: true }).click();
    const prepared = (await facts()).source;
    assert(prepared && prepared.mode === 'paid' && prepared.acceptance === 'recoverable');
    expect(prepared.payment.total).toBe('17');
    expect(prepared.payment.chain_id).toBe('31337');
    expect(prepared.payment.gas_cap).toBe(environment.gasCap);
    assert(prepared.payment.asset && prepared.payment.provider_wallet);
    await expect(details.getByText('31337', { exact: true })).toBeVisible();
    await expect(details.getByText(prepared.payment.asset, { exact: true })).toBeVisible();
    await expect(details.getByText(prepared.payment.provider_wallet, { exact: true })).toBeVisible();
    await expect(details.getByText('服务费合计', { exact: true })).toBeVisible();
    await expect(details.getByText('17', { exact: true })).toBeVisible();
    await expect(details.getByText(environment.sha256, { exact: true })).toBeVisible();
    await details.getByText('服务费合计', { exact: true }).scrollIntoView();
    await event('prepared');
    const quotationScreenshot = await preserveScreenshot('web.png', await app.screenshot('ordinary-paid-quotation'));
    const recording = await webRecording();
    try {
      await screen.getByRole('button', '确认并付款').click();
      await expect.poll(async () => (await facts()).source?.phase !== 'prepared', {
        timeout: 30000, message: 'The actual Web member authorizes the original paid intent',
      }).toBe(true);
      await event('confirmed');
      await browser.reload();
      const reloaded = screen.getByRole('region', '传输详情', { exact: true });
      await expect(reloaded).toBeVisible({ timeout: 30000 });
      await reloaded.getByText('详细信息', { exact: true }).click();
      await expect(screen.getByText(prepared.order_id, { exact: true })).toBeVisible();
      await expect(screen.getByText(prepared.intent_id, { exact: true })).toBeVisible();
      await videoEvidence('web', prepared.order_id, prepared.intent_id, [await recording.stop()]);
    } finally { await recording.cancel(); }
    await expect.poll(async () => {
      const current = await facts();
      assert(current.source?.order_id === prepared.order_id && current.source.intent_id === prepared.intent_id);
      return current.events.includes('offline-visible') && current.settled
        && current.source.phase === 'paid' && current.source.evidence === 'decryption_proven'
        && current.source.payment.earned === '17' && current.source.payment.withdrawn === '17' && current.source.payment.refunded === '0';
    }, { timeout: ordinaryDeadline(), interval: 1000, message: 'Actual Nodes settle while the Android app is offline and backgrounded' }).toBe(true);
    const settled = await facts();
    assert(settled.provider);
    expect(settled.provider.payment.earned).toBe('17');
    expect(settled.provider.payment.withdrawn).toBe('17');
    expect(settled.provider.payment.refunded).toBe('0');
    assert(settled.source?.order_id === prepared.order_id && settled.source.intent_id === prepared.intent_id
      && settled.source.evidence === 'decryption_proven');
    const completed = screen.getByRole('region', '传输详情', { exact: true });
    await expect(completed.getByText(prepared.order_id, { exact: true })).toBeVisible({ timeout: 30000 });
    await expect(completed.getByText(prepared.intent_id, { exact: true })).toBeVisible();
    await expect(completed.getByText('已完成 · 服务费已提取', { exact: true })).toBeVisible();
    await expect(completed.getByText('对方已打开。交付确认不表示真人已阅读。', { exact: true })).toBeVisible();
    await expect(completed.getByText('17 / 17 / 0', { exact: true })).toBeVisible();
    await expect(completed.getByText(/^窗口 1 .*已证明解密$/)).toBeVisible();
    await browser.setViewport({ width: 1440, height: 2200 });
    await completed.scrollIntoView();
    await preserveScreenshot('web-settled.png', await app.screenshot('ordinary-paid-settled'));
    await browser.setViewport({ width: 1440, height: 1000 });
    await saveEvidence('web.json', { status: 'passed', orderId: prepared.order_id, intentId: prepared.intent_id,
      bytes: environment.bytes, sha256: environment.sha256, actualPairing: true, actualFileSelection: true,
      actualQuotation: true, actualPaymentConfirmation: true, reloadPreservedIntent: true,
      settledWhileReceiverAppOffline: true, earned: '17', withdrawn: '17', refunded: '0', quotationScreenshot });
    await event('settled-offline');
  }));
