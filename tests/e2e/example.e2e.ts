import { test } from './fixtures';
import { expect } from 'e2e';

test('app opens', async ({ app, node, screen }) => {
  await app.open(node.url);
  await expect(screen.getByRole('button', '新建我的网络')).toBeVisible();
  await expect(screen.getByRole('button', '加入已有网络')).toBeVisible();
});
