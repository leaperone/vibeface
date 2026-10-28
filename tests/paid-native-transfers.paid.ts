import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { createHash } from 'node:crypto';
import { test } from '@e2e-dev/mobile';
import { expect, secrets } from 'e2e';
import { acknowledgeInspection, deadline, event, facts, nextInspection, ordinaryDeadline, ordinaryReady, pairing, preserveScreenshot, request, runUi, saveEvidence } from './paid-delivery-control';
import { androidRecording, type Video, videoEvidence } from './paid-recordings';

const execute = promisify(execFile);
const appId = 'one.leaper.vibeface';

test('accepts a paid file in the release Android app, disconnects, resumes and saves the original bytes',
  { platforms: ['android'], timeout: deadline() }, async ({ app, device, screen }) => runUi('android', async () => {
    const serial = process.env.VIBEFACE_E2E_ANDROID_SERIAL;
    assert(serial && /^emulator-[0-9]+$/.test(serial), 'Android acceptance always names its dedicated AVD');
    const adb = async (arguments_: string[]) => (await execute('adb', ['-s', serial, ...arguments_], { encoding: 'utf8', timeout: 15000, maxBuffer: 65536 })).stdout.trim();
    const background = async () => {
      const foreground = await device.foregroundApp();
      assert(typeof foreground.bundleId === 'string' && foreground.bundleId.length > 0 && foreground.bundleId !== appId,
        'The actual Android foreground package identifies a different app');
    };
    assert.equal(await adb(['shell', 'id', '-u']), '0', 'Only this task root AVD may inspect the saved transfer file');
    const environment = await facts();
    const receiverPort = new URL(environment.nodes.android.url).port;
    assert(/^[0-9]+$/.test(receiverPort));
    const reverse = `tcp:${receiverPort}`;
    const mappings = async () => (await adb(['reverse', '--list'])).split('\n').map(line => line.trim().split(/\s+/));
    const reversePresent = async () => (await mappings()).some(columns => columns[1] === reverse && columns[2] === reverse);
    await adb(['reverse', reverse, reverse]);
    assert(await reversePresent(), 'The task receiver has one explicit loopback reverse');
    try {
      await device.installApp(undefined, { reinstall: true });
      await app.open();
      await screen.getByRole('textbox', /^加入链接/).fill(secrets.get('paidAndroidInvitation'), { timeout: 30000 });
      await screen.getByRole('textbox', /^这台手机叫什么/).fill('VDP paid native receiver');
      await device.dismissKeyboard();
      await screen.getByRole('button', '加入', { exact: true }).click();
      await expect.poll(async () => (await pairing('android')) !== null, { timeout: 45000, message: 'The real Android member reveals its pairing nonce' }).toBe(true);
      const joined = await pairing('android');
      assert(joined);
      await expect(screen.getByLabel(`安全码 ${joined.code.split('').join(' ')}`, { exact: true })).toBeVisible();
      await request('/approve', { role: 'android', peer: joined.peer, code: joined.code });
      await expect(screen.getByRole('link', '设置', { exact: true })).toBeVisible({ timeout: 30000 });
      await screen.getByRole('link', '设置', { exact: true }).click();
      await screen.scrollUntilVisible(screen.getByRole('link', '发送和接收文件', { exact: true }), { direction: 'down' });
      await screen.getByRole('link', '发送和接收文件', { exact: true }).click();
      await expect(screen.getByText('传输设置', { exact: true }).first()).toBeVisible();
      await event('observer-ready', { role: 'android' });
      for (const kind of ['receiver-key-before', 'receiver-key-after'] as const) {
        const inspection = await nextInspection(kind);
        assert(inspection.role === 'android' && inspection.nodePeer === environment.nodes.android.peer && inspection.keyId
          && inspection.keySerial === (kind === 'receiver-key-before' ? 1 : 2));
        if (kind === 'receiver-key-after') {
          await device.closeApp();
          await device.openApp(appId);
          await device.openLink('vibeface://transfers', { app: appId });
        }
        const refresh = screen.getByRole('button', '刷新', { exact: true });
        await screen.scrollUntilVisible(refresh, { direction: 'up', timeout: 30000 });
        await refresh.click();
        const settingsDetails = screen.getByRole('button', '详细信息', { exact: true }).first();
        await screen.scrollUntilVisible(settingsDetails, { direction: 'up', timeout: 30000 });
        await settingsDetails.click();
        const key = screen.getByText(inspection.keyId, { exact: true }).first();
        await screen.scrollUntilVisible(key, { direction: 'down', timeout: 30000 });
        await expect(screen.getByText('当前接收密钥 ID', { exact: true }).first()).toBeVisible();
        await expect(key).toBeVisible();
        await expect(screen.getByText('当前接收密钥序号', { exact: true }).first()).toBeVisible();
        await expect(screen.getByText(inspection.keySerial.toString(), { exact: true }).first()).toBeVisible();
        await acknowledgeInspection(inspection, await app.screenshot(kind));
      }
      await ordinaryReady();
      await device.closeApp();
      await device.openApp(appId);
      await device.openLink('vibeface://transfers', { app: appId });
      await expect(screen.getByText('传输设置', { exact: true }).first()).toBeVisible({ timeout: 45000 });
      await expect.poll(async () => (await facts()).receiver !== null, {
        timeout: ordinaryDeadline(), interval: 1000, message: 'The original ordinary order reaches the real receiver after its genuine source proof',
      }).toBe(true);
      const offered = (await facts()).receiver;
      assert(offered && offered.acceptance === 'recoverable' && offered.mode === 'paid');
      const incoming = screen.getByRole('button', /^待接收的文件/).first();
      await screen.scrollUntilVisible(incoming, { direction: 'down', timeout: 30000 });
      await incoming.click();
      const accept = screen.getByRole('button', '接收文件', { exact: true });
      await screen.scrollUntilVisible(accept, { direction: 'down' });
      const videos: Video[] = [];
      const acceptanceRecording = await androidRecording('android-accept-offline', serial);
      let offlineScreenshot: string;
      try {
        await accept.click();
        await expect.poll(async () => {
          const result = await request('/event', { name: 'accepted' });
          return result !== null;
        }, { timeout: 30000, message: 'The actual Android manager authorizes receiver acceptance' }).toBe(true);
        await device.home();
        await background();
        await adb(['reverse', '--remove', reverse]);
        assert(!(await reversePresent()), 'Only the task receiver reverse is removed');
        await device.closeApp();
        await device.openApp(appId);
        const offline = screen.getByLabel(/^连接：(.* · )?离线$/);
        await expect(offline).toBeVisible({ timeout: 45000 });
        assert(!(await reversePresent()), 'The real app reports a failed connection while its receiver reverse remains absent');
        await offline.click({ timeout: 30000 });
        const offlineCard = screen.getByLabel(/^(?!连接：)(.* · )?离线$/);
        await expect(offlineCard).toBeVisible({ timeout: 30000 });
        offlineScreenshot = await preserveScreenshot('android-offline.png', await app.screenshot('ordinary-paid-android-offline'));
        await device.back();
        await expect(offlineCard).toBeHidden({ timeout: 30000 });
        await device.home();
        await background();
        await event('offline-visible');
        videos.push(await acceptanceRecording.stop());
      } finally { await acceptanceRecording.cancel(); }
      await expect.poll(async () => (await facts()).events.includes('settled-offline'), {
        timeout: ordinaryDeadline(), interval: 1000, message: 'Actual Node settlement completes while this app stays backgrounded and disconnected',
      }).toBe(true);
      assert(!(await reversePresent()));
      await background();
      const resumeRecording = await androidRecording('android-resume-save', serial);
      try {
        await adb(['reverse', reverse, reverse]);
        assert(await reversePresent());
        await device.openApp(appId);
        await device.openLink('vibeface://transfers', { app: appId });
        await expect(screen.getByText('传输设置', { exact: true }).first()).toBeVisible({ timeout: 45000 });
        const recovered = screen.getByRole('button', new RegExp(`^${environment.name.replaceAll('.', '\\.')}`));
        await screen.scrollUntilVisible(recovered, { direction: 'down', timeout: 30000 });
        await recovered.click();
        const orderDetails = screen.getByRole('button', '详细信息', { exact: true }).last();
        await screen.scrollUntilVisible(orderDetails, { direction: 'down' });
        await orderDetails.click();
        await screen.scrollUntilVisible(screen.getByText(offered.order_id, { exact: true }).first(), { direction: 'down' });
        await expect(screen.getByText(offered.order_id, { exact: true }).first()).toBeVisible();
        await screen.scrollUntilVisible(screen.getByText(offered.intent_id, { exact: true }).first(), { direction: 'down' });
        await expect(screen.getByText(offered.intent_id, { exact: true }).first()).toBeVisible();
        const download = screen.getByRole('button', '保存文件', { exact: true });
        await screen.scrollUntilVisible(download, { direction: 'down' });
        await download.click();
        const directory = `/data/user/0/${appId}/files/transfers`;
        const savedFile = async () => {
          const output = await adb(['shell', `find ${directory} -maxdepth 1 -type f -name '*-ordinary-paid-ui.bin'`]);
          const matches = output.split('\n').filter(Boolean);
          if (matches.length !== 1) return null;
          assert(new RegExp(`^${directory}/[a-f0-9-]{36}-ordinary-paid-ui\\.bin$`).test(matches[0]), 'Read only the actual app transfer file');
          const saved = (await execute('adb', ['-s', serial, 'exec-out', 'cat', matches[0]], { encoding: null, timeout: 10000, maxBuffer: 8192 })).stdout;
          return { bytes: saved.length, sha256: createHash('sha256').update(saved).digest('hex') };
        };
        await expect.poll(async () => (await savedFile())?.sha256 ?? null, {
          timeout: 30000, message: 'The actual release app saves the original Android transfer bytes',
        }).toBe(environment.sha256);
        const saved = await savedFile();
        assert(saved && saved.bytes === 4096 && saved.sha256 === environment.sha256);
        await device.back();
        const notice = screen.getByText('文件校验通过，并已保存到此应用。分享面板可导出副本。', { exact: true }).first();
        await screen.scrollUntilVisible(notice, { direction: 'up', timeout: 30000 });
        await expect(notice).toBeVisible();
        const savedScreenshot = await preserveScreenshot('android.png', await app.screenshot('ordinary-paid-android-saved'));
        videos.push(await resumeRecording.stop());
        await videoEvidence('android', offered.order_id, offered.intent_id, videos);
        await saveEvidence('android.json', { status: 'passed', orderId: offered.order_id, intentId: offered.intent_id,
          bytes: saved.bytes, sha256: saved.sha256, actualPairing: true, actualAcceptance: true, backgroundObserved: true,
          receiverReverseRemoved: true, existingAppConnectionTerminated: true, actualOfflineVisible: true,
          settledWhileBackgrounded: true, sameOrderResumed: true, actualSavedFile: true, offlineScreenshot, savedScreenshot });
        await event('saved', { bytes: saved.bytes, sha256: saved.sha256 });
      } finally { await resumeRecording.cancel(); }
    } finally {
      await adb(['reverse', '--remove', reverse]).catch(() => undefined);
    }
  }));
