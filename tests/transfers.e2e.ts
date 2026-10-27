import { createHash, randomBytes } from 'node:crypto';
import { readFile, writeFile } from 'node:fs/promises';
import { join } from 'node:path';
import type { TestFixtures } from 'e2e';
import { expect, secrets } from 'e2e';
import type { Browser } from '@e2e-dev/web';
import { deliveryTest as test } from './fixtures';

type Page = Pick<TestFixtures, 'screen'> & { browser: Browser };
async function createNetwork({ screen, browser }: Page, url: string, name: string) {
  await browser.goto(url);
  await screen.getByLabel('这台电脑叫什么？').fill(name);
  await screen.getByRole('button', '下一步').click();
  await expect(screen.getByRole('heading', '设置恢复密码')).toBeVisible();
  await screen.getByLabel(/^恢复密码/).fill(secrets.get('recoveryPassword'));
  await screen.getByRole('button', '下一步').click();
  await expect(screen.getByRole('heading', '保存恢复文件')).toBeVisible();
  await browser.waitForDownload(() => screen.getByRole('button', '下载恢复文件').click());
  await screen.getByRole('checkbox', '已把文件和密码分开保存').check();
  await screen.getByRole('button', '完成创建').click();
  await expect(screen.getByRole('heading', '让这台电脑运行 Bot')).toBeVisible();
  await screen.getByRole('button', '稍后设置').click();
  await expect(screen.getByRole('heading', '创建第一个 Bot')).toBeVisible();
  await screen.getByRole('button', '跳过', { exact: true }).click();
  await browser.goto(`${url}/transfers`);
  await expect(screen.getByRole('heading', '传输设置', { exact: true })).toBeVisible();
  await expect(screen.getByRole('switch', '启用文件传输')).not.toBeChecked();
  await screen.getByRole('switch', '启用文件传输').check();
  await expect(screen.getByRole('heading', '发送文件')).toBeVisible();
}

test('forwards an encrypted charity file between separate networks and survives a receiver restart', { timeout: 180000 }, async ({ network, browser, screen, app }) => {
  await createNetwork({ browser, screen }, network.receiver.url, 'E2E Receiver');
  await createNetwork({ browser, screen }, network.source.url, 'E2E Sender');
  await screen.getByLabel('接收方的连接 ID').fill(network.receiver.peer);
  await screen.getByLabel('中转设备的连接 ID').fill(network.provider.peer);
  await expect(screen.getByText('全程加密 · 免费', { exact: true })).toBeVisible();
  const bytes = randomBytes(96 * 1024), name = 'charity-original.bin';
  const input = join(network.source.directory, name);
  await writeFile(input, bytes, { mode: 0o600, flag: 'wx' });
  const digest = createHash('sha256').update(bytes).digest('hex');
  await browser.evaluate(() => {
    const intercept = (event: MouseEvent) => {
      if (event.target instanceof HTMLInputElement && event.target.type === 'file' && event.target.parentElement === document.body) {
        event.preventDefault();
        document.removeEventListener('click', intercept, true);
      }
    };
    document.addEventListener('click', intercept, true);
    return null;
  });
  await screen.getByRole('button', '选择文件').click();
  await browser.locator('body > input[type="file"]').setInputFiles(input);
  await expect(screen.getByText(name, { exact: true })).toBeVisible();
  await screen.getByRole('button', '下一步').click();
  await expect(screen.getByRole('heading', '确认发送')).toBeVisible();
  await screen.getByRole('button', '确认发送').click();
  await browser.goto(`${network.receiver.url}/transfers`);
  await screen.getByRole('button', '刷新', { exact: true }).click();
  await expect(screen.getByRole('button', /^待接收的文件/)).toBeVisible({ timeout: 30000 });
  await screen.getByRole('button', /^待接收的文件/).click();
  await screen.getByRole('button', '接收文件').click();
  await expect(screen.getByRole('button', '刷新', { exact: true })).toBeEnabled();
  await network.restart('receiver', true);
  await browser.reload();
  await expect(screen.getByRole('heading', '传输设置', { exact: true })).toBeVisible();
  await expect(screen.getByRole('button', name, { exact: false })).toBeVisible({ timeout: 30000 });
  await screen.getByRole('button', name, { exact: false }).click();
  const download = await browser.waitForDownload(() => screen.getByRole('button', '保存文件').click(), { timeout: 30000 });
  expect(createHash('sha256').update(await readFile(download.absolutePath)).digest('hex')).toBe(digest);
  await expect(screen.getByRole('heading', '传输详情')).toBeVisible();
  await app.screenshot('charity-file-recovered');
  const state = await network.inspect();
  expect(state.wallet_files).toBe(0);
  expect(state.chain_rpc_calls).toBe(0);
  expect(state.pkarr_reads).toBeGreaterThan(0);
  expect(state.pkarr_writes).toBeGreaterThan(0);
});
