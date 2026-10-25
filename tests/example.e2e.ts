import { test } from '@e2e-dev/web';
import { expect } from 'e2e';

test('app opens', async ({ app, screen }) => {
  await app.open('/');
  await expect(screen.getByRole('heading', '给这台电脑起名')).toBeVisible();
});
