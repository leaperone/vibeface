import { expect } from 'e2e';
import { bootstrapTest as test } from './fixtures';

test('joins through the page node when Core bootstraps from its discovery', async ({ app, browser, screen, bootstrap }) => {
  await app.open('/join');
  await screen.getByLabel('加入链接', { exact: true }).fill(bootstrap.invitation);
  await screen.getByLabel('这个浏览器叫什么？', { exact: true }).fill('Bootstrap browser');
  await screen.getByRole('button', '加入', { exact: true }).click();
  const pairing = await bootstrap.pairing();
  await expect(screen.getByLabel(`安全码 ${pairing.code.split('').join(' ')}`, { exact: true })).toBeVisible();
  await bootstrap.approve(pairing);
  await expect(screen.getByRole('button', 'Bootstrap fixture assistant', { exact: true })).toBeVisible({ timeout: 30000 });
  await browser.reload();
  await expect(screen.getByRole('button', 'Bootstrap fixture assistant', { exact: true })).toBeVisible({ timeout: 30000 });
});
