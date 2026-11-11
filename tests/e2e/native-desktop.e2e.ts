import assert from 'node:assert/strict';
import { test } from 'e2e';
import { nativeDesktopAcceptance } from './native-desktop-fixture';

test('packaged GPUI window receives native input, executes Enter and exits without starting Node', { platforms: ['native-desktop'] }, async ({ platform }) => {
  assert.equal(platform, 'native-desktop');
  const evidence = await nativeDesktopAcceptance();
  console.log(`Native OS window, keyboard and PNG evidence: ${evidence}`);
});
