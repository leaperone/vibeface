import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { createHash } from 'node:crypto';
import { resolve } from 'node:path';
import { test } from '@e2e-dev/web';
import { expect, secrets } from 'e2e';
import { deadline, event, facts, insideArtifacts, pairing, preserveScreenshot, request, root, runUi, saveEvidence } from './paid-delivery-control';
import { videoEvidence, webRecording } from './paid-recordings';

test('quotes and pays one recoverable file through the real Web UI while Android recovers offline',
  { platforms: ['web'], timeout: deadline() }, async ({ browser, screen, app }) => runUi('web', async () => {
    const environment = await facts();
    await browser.goto(`${environment.nodes.web.url}/join`);
    await screen.getByLabel('加入链接', { exact: true }).fill(secrets.get('paidWebInvitation'), { timeout: 30000 });
    await screen.getByLabel('这个浏览器叫什么？', { exact: true }).fill('VDP paid web sender');
    await screen.getByRole('button', '继续', { exact: true }).click();
    await expect.poll(async () => (await pairing('web')) !== null, { timeout: 45000, message: 'The real Web member reveals its pairing nonce' }).toBe(true);
    const joined = await pairing('web');
    assert(joined);
    await expect(screen.getByLabel(`安全码 ${joined.code.split('').join(' ')}`, { exact: true })).toBeVisible();
    await request('/approve', { role: 'web', peer: joined.peer, code: joined.code });
    await browser.goto(`${environment.nodes.web.url}/transfers`);
    await expect(screen.getByRole('heading', '此设备的传输设置')).toBeVisible({ timeout: 30000 });
    await expect(screen.getByRole('switch', '付费交付（本地测试）')).toBeChecked();
    await expect.poll(async () => (await facts()).paired.android, { timeout: 180000, message: 'The Android receiver joins through its actual invitation' }).toBe(true);
    await screen.getByLabel('接收设备的连接 ID').fill(environment.nodes.android.peer);
    await screen.getByLabel('服务设备的连接 ID').fill(environment.nodes.provider.peer);
    await screen.getByRole('radio', '付费交付', { exact: true }).click();
    await expect(screen.getByRole('radio', '确认可恢复后结算')).toBeChecked();
    await screen.getByLabel('服务费上限（测试代币最小单位）').fill(environment.serviceCap);
    await screen.getByLabel('链费上限（测试链原生币最小单位）').fill(environment.gasCap);
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
    await expect(screen.getByText(`${environment.name} · 4096 字节。当前最多`, { exact: false })).toBeVisible();
    await screen.getByRole('button', '准备订单并查看费用').click();
    const details = screen.getByRole('region', '确认这笔订单', { exact: true });
    await expect(details).toBeVisible({ timeout: 45000 });
    const prepared = (await facts()).source;
    assert(prepared && prepared.mode === 'paid' && prepared.acceptance === 'recoverable');
    expect(prepared.payment.total).toBe('17');
    expect(prepared.payment.chain_id).toBe('31337');
    expect(prepared.payment.gas_cap).toBe(environment.gasCap);
    assert(prepared.payment.asset && prepared.payment.provider_wallet);
    await expect(details.getByText('31337', { exact: true })).toBeVisible();
    await expect(details.getByText(prepared.payment.asset, { exact: true })).toBeVisible();
    await expect(details.getByText(prepared.payment.provider_wallet, { exact: true })).toBeVisible();
    await expect(details.getByText('服务费合计（最小单位）', { exact: true })).toBeVisible();
    await expect(details.getByText('17', { exact: true })).toBeVisible();
    await expect(details.getByText(environment.sha256, { exact: true })).toBeVisible();
    await details.getByText('服务费合计（最小单位）', { exact: true }).scrollIntoView();
    await event('prepared');
    const quotationScreenshot = await preserveScreenshot('web.png', await app.screenshot('ordinary-paid-quotation'));
    const recording = await webRecording();
    try {
      await screen.getByRole('button', '确认费用并提交原订单').click();
      await expect.poll(async () => (await facts()).source?.phase !== 'prepared', {
        timeout: 30000, message: 'The actual Web member authorizes the original paid intent',
      }).toBe(true);
      await event('confirmed');
      await browser.reload();
      await expect(screen.getByText(prepared.order_id, { exact: true })).toBeVisible({ timeout: 30000 });
      await expect(screen.getByText(prepared.intent_id, { exact: true })).toBeVisible();
      await videoEvidence('web', prepared.order_id, prepared.intent_id, [await recording.stop()]);
    } finally { await recording.cancel(); }
    await expect.poll(async () => {
      const current = await facts();
      assert(current.source?.order_id === prepared.order_id && current.source.intent_id === prepared.intent_id);
      return current.events.includes('offline-visible') && current.settled;
    }, { timeout: deadline(), interval: 1000, message: 'Actual Nodes settle while the Android app is offline and backgrounded' }).toBe(true);
    await event('settled-offline');
    const settled = await facts();
    assert(settled.provider);
    expect(settled.provider.payment.earned).toBe('17');
    expect(settled.provider.payment.withdrawn).toBe('17');
    expect(settled.provider.payment.refunded).toBe('0');
    await saveEvidence('web.json', { status: 'passed', orderId: prepared.order_id, intentId: prepared.intent_id,
      bytes: environment.bytes, sha256: environment.sha256, actualPairing: true, actualFileSelection: true,
      actualQuotation: true, actualPaymentConfirmation: true, reloadPreservedIntent: true,
      settledWhileReceiverAppOffline: true, earned: '17', withdrawn: '17', refunded: '0', quotationScreenshot });
  }));
