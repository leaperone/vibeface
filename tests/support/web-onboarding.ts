import type { Browser } from '@e2e-dev/web';
import type { TestFixtures } from 'e2e';
import { secrets } from 'e2e';

/** The Web driver surface needed by the shared personal-network actions. */
export type WebOnboardingPage = Pick<TestFixtures, 'screen'> & { browser: Browser };

/** Start the create-network form and leave it on the recovery-password step. */
export async function createPersonalNetwork({ screen }: WebOnboardingPage, name: string): Promise<void> {
  await screen.getByRole('button', '新建我的网络').click();
  await screen.getByLabel('这个节点叫什么？').fill(name);
  await screen.getByRole('button', '下一步').click();
}

/** Submit the recovery password, entered twice, and leave the recovery-file step ready for download. */
export async function continueRecoverySetup({ screen }: WebOnboardingPage): Promise<void> {
  await screen.getByLabel(/^恢复密码/).fill(secrets.get('recoveryPassword'));
  await screen.getByLabel('再输入一遍恢复密码').fill(secrets.get('recoveryPassword'));
  await screen.getByRole('button', '下一步').click();
}

/** Download and acknowledge the recovery file, then finish network creation. */
export async function finishRecoveryDownload({ screen, browser }: WebOnboardingPage): Promise<Awaited<ReturnType<Browser['waitForDownload']>>> {
  const download = await browser.waitForDownload(() => screen.getByRole('button', '下载恢复文件').click());
  await screen.getByRole('checkbox', '已把文件和密码分开保存').check();
  await screen.getByRole('button', '完成创建').click();
  return download;
}
