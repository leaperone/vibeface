import { test } from './fixtures';
import { expect } from 'e2e';

test('app opens', async ({ app, node, screen }) => {
  await app.open(node.url);
  await expect(screen.getByRole('heading', '给这台电脑起名')).toBeVisible();
});
