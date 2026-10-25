import { stat } from 'node:fs/promises';
import { test } from '@e2e-dev/web';
import { expect, secrets } from 'e2e';

test('creates a network and a persistent Bot without a model', async ({ app, screen, browser }) => {
  await app.open('/');
  await screen.getByLabel('这台电脑叫什么？').fill('E2E Node');
  await screen.getByRole('button', '下一步').click();
  await expect(screen.getByRole('heading', '设置恢复密码')).toBeVisible();
  await screen.getByLabel(/^恢复密码/).fill(secrets.get('recoveryPassword'));
  await screen.getByRole('button', '下一步').click();
  await expect(screen.getByRole('heading', '保存恢复文件')).toBeVisible();
  const download = await browser.waitForDownload(() => screen.getByRole('button', '下载恢复文件').click());
  expect((await stat(download.absolutePath)).size).toBeGreaterThan(0);
  await screen.getByRole('checkbox', '已把文件和密码分开保存').check();
  await screen.getByRole('button', '完成创建').click();
  await expect(screen.getByRole('heading', '让这台电脑运行 Bot')).toBeVisible();
  await screen.getByRole('button', '稍后设置').click();
  await screen.getByLabel('名称', { exact: true }).fill('E2E Bot');
  await screen.getByRole('button', '开始对话').click();
  await expect(browser).toHaveURL(/\/b\/[^/?#]+$/);
  const bot = screen.getByRole('region', '对话').getByRole('heading', 'E2E Bot');
  await expect(bot).toBeVisible();
  await browser.reload();
  await expect(bot).toBeVisible();
  await app.screenshot('onboarding-complete');
});
