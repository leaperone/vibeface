import { expect } from 'e2e';
import { meshTest as test } from './fixtures';

test('reaches a member Node by device ID through the page Node, its mesh bootstrap node', async ({ browser, app, screen, mesh }) => {
  await app.open('/join');
  await screen.getByLabel('加入链接', { exact: true }).fill(mesh.invitation);
  await screen.getByLabel('这个浏览器叫什么？', { exact: true }).fill('Mesh browser');
  await screen.getByRole('button', '加入', { exact: true }).click();
  const pairing = await mesh.pairing();
  await expect(screen.getByLabel(`安全码 ${pairing.code.split('').join(' ')}`, { exact: true })).toBeVisible();
  await mesh.approve(pairing);
  await expect(screen.getByRole('button', 'Mesh fixture assistant', { exact: true })).toBeVisible({ timeout: 30000 });
  await browser.reload();
  await expect(screen.getByRole('button', 'Mesh fixture assistant', { exact: true })).toBeVisible({ timeout: 30000 });
});
