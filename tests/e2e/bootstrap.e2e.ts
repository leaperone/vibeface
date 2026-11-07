import assert from 'node:assert/strict';
import { expect } from 'e2e';
import { bootstrapTest, httpBootstrapTest } from './fixtures';

for (const [test, via] of [[bootstrapTest, 'udp'], [httpBootstrapTest, 'https']] as const) {
  test(`joins through a signed public gateway when Core bootstraps over ${via}`, async ({ app, browser, screen, bootstrap }) => {
    assert.equal(bootstrap.via, via);
    assert.equal(bootstrap.memberHasOperatorToken, false);
    const unavailable = await fetch(`${bootstrap.site}/api/client`);
    assert.equal(unavailable.status, 503, 'The page origin cannot proxy a client connection');
    await app.open('/join');
    await screen.getByLabel('加入链接', { exact: true }).fill(bootstrap.invitation);
    await screen.getByLabel('这个浏览器叫什么？', { exact: true }).fill('Bootstrap browser');
    await screen.getByRole('button', '加入', { exact: true }).click();
    const pairing = await bootstrap.pairing();
    await expect(screen.getByLabel(`安全码 ${pairing.code.split('').join(' ')}`, { exact: true })).toBeVisible();
    await bootstrap.approve(pairing);
    await expect(screen.getByRole('button', 'Bootstrap fixture assistant', { exact: true })).toBeVisible({ timeout: 30000 });
    const after = await bootstrap.inspect();
    assert(after.catalogReads > 0, 'The browser read the actual signed Discovery catalogue');
    assert.equal(after.memberHasOperatorToken, false);
    await browser.reload();
    await expect(screen.getByRole('button', 'Bootstrap fixture assistant', { exact: true })).toBeVisible({ timeout: 30000 });
  });
}
