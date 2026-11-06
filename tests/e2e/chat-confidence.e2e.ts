import assert from 'node:assert/strict';
import { join } from 'node:path';
import { DatabaseSync } from 'node:sqlite';
import { test } from './fixtures';
import { expect, secrets } from 'e2e';

test('chat keeps decisions and task records readable on desktop and phone', async ({ app, node, screen, browser }) => {
  await app.open(node.url);
  await screen.getByRole('button', '新建我的网络').click();
  await screen.getByLabel('这个节点叫什么？').fill('交互验收设备');
  await screen.getByRole('button', '下一步').click();
  await screen.getByLabel(/^恢复密码/).fill(secrets.get('recoveryPassword'));
  await screen.getByRole('button', '下一步').click();
  await browser.waitForDownload(() => screen.getByRole('button', '下载恢复文件').click());
  await screen.getByRole('checkbox', '已把文件和密码分开保存').check();
  await screen.getByRole('button', '完成创建').click();
  await screen.getByRole('button', '稍后设置').click();
  await screen.getByLabel('名称', { exact: true }).fill('运营助手');
  await screen.getByRole('button', '完成', { exact: true }).click();
  await expect(browser).toHaveURL(/\/b\/[^/?#]+$/);

  // Controlled read-model fixtures, as in onboarding.e2e.ts. These do not run an Agent.
  const db = new DatabaseSync(join(node.directory, 'node.sqlite3'));
  try {
    const identity = db.prepare('SELECT network_id,conversation_id,bot_id FROM bots WHERE name=?').get('运营助手');
    assert(identity && typeof identity.network_id === 'string' && typeof identity.conversation_id === 'string' && typeof identity.bot_id === 'string');
    const { network_id: network, conversation_id: conversation, bot_id: bot } = identity;
    const work = 'chat-confidence-work';
    const dispatch = { type: 'work_dispatched', work_id: work, trigger: 'message:report', dispatcher: node.peer, call_ref: 'report', kind: 'task', resolved: { provider: 'codex', model: null }, cwd: 'workspace', title: '整理本月运营情况', prompt: '整理本月运营情况，标明尚未确认的部分。' };
    const summary = '已汇总现有记录。\n正在检查退款是否单独统计。\n详细资料保留在这里，不需要默认展开。';
    const progress = { type: 'work_progress', work_id: work, phase: 'running', summary };
    db.prepare('INSERT INTO works(network_id,work_id,conversation_id,target_node,dispatch,dispatch_rank,progress,progress_rank) VALUES(?,?,?,?,?,?,?,?)').run(network, work, conversation, node.peer, JSON.stringify(dispatch), '1', JSON.stringify(progress), '2');
    const question = { type: 'question_asked', question_id: 'report-scope', target: { kind: 'bot_main', bot_id: bot }, attention: { kind: 'question', device_id: null }, severity: 'attention', text: '退款需要单独统计吗？\n选择后会按这个口径整理。', choices: [{ label: '单独统计' }, { label: '合并统计' }], about: { node: node.peer, agent: null, session_id: 'report', cwd: 'workspace', summary: '' } };
    db.prepare('INSERT INTO questions(network_id,question_id,conversation_id,rank,author,created_at_ms,node,session_id,body) VALUES(?,?,?,?,?,?,?,?,?)').run(network, question.question_id, conversation, '3', node.peer, Date.now(), node.peer, 'report', JSON.stringify(question));
    await browser.reload();
    await expect(screen.getByRole('heading', '退款需要单独统计吗？')).toBeVisible();
    await expect(screen.getByText('选择后会按这个口径整理。', { exact: true })).toBeVisible();
    await expect(screen.getByRole('radio', '单独统计', { exact: true })).toBeVisible();
    await app.screenshot('chat-confidence-decision-desktop');

    await browser.goto(`${node.url}/b/${bot}/work/${work}`);
    const desktopTask = screen.getByRole('complementary', '任务详情');
    await expect(desktopTask.getByText('详细进展', { exact: true })).toBeVisible();
    const detailOpen = () => browser.evaluate(() => [...document.querySelectorAll('details')].find(item => item.querySelector('summary')?.textContent?.includes('详细进展'))?.open ?? null);
    assert.equal(await detailOpen(), false);
    await desktopTask.getByText('详细进展', { exact: true }).click();
    assert.equal(await detailOpen(), true);
    await expect(desktopTask.getByText(summary, { exact: true })).toBeVisible();
    await expect(screen.getByRole('textbox', '给 Bot 发消息')).toBeVisible();
    await expect(desktopTask.getByRole('textbox', '补充给 整理本月运营情况')).toBeVisible();
    const mainInput = await browser.evaluate(() => ({
      count: document.querySelectorAll('textarea[id="message-text"]').length,
      focusLabel: document.getElementById('message-text')?.getAttribute('aria-label') ?? null,
    }));
    assert.deepEqual(mainInput, { count: 1, focusLabel: '给 Bot 发消息' });
    await app.screenshot('chat-confidence-progress-desktop');

    const finish = { type: 'work_finished', work_id: work, outcome: 'unverified', result_text: '', failure_reason: '这次的结果无法确认。' };
    db.prepare('UPDATE works SET finish=?,finish_rank=? WHERE network_id=? AND work_id=?').run(JSON.stringify(finish), '4', network, work);
    await browser.setViewport({ width: 390, height: 844 });
    await browser.reload();
    const task = screen.getByRole('dialog');
    await expect(task.getByRole('heading', '还不知道做完没有')).toBeVisible();
    await expect(task.getByRole('button', '先查结果', { exact: true })).toBeVisible();
    await expect(task.getByRole('button', '做完了', { exact: true })).not.toBeVisible();
    await task.getByText('我已自行核对', { exact: true }).click();
    await expect(task.getByRole('button', '做完了', { exact: true })).toBeVisible();
    await expect(task.getByRole('button', '没做完', { exact: true })).toBeVisible();
    const geometry = await browser.evaluate(() => ({ width: document.documentElement.clientWidth, content: document.documentElement.scrollWidth }));
    assert(geometry.content <= geometry.width, 'phone layout must not scroll horizontally');
    const blockedReason = '这个工作当前不能接收补充。选择「新任务」继续。';
    const blockedHint = task.getByText(blockedReason, { exact: true });
    await expect(blockedHint).toBeVisible();
    const hintBounds = await blockedHint.boundingBox();
    assert(hintBounds, 'The blocked input reason renders');
    assert(hintBounds.x >= 0 && hintBounds.y >= 0 && hintBounds.x + hintBounds.width <= 390 && hintBounds.y + hintBounds.height <= 844, 'The full blocked input reason fits the phone viewport');
    const hintId = await blockedHint.getAttribute('id');
    assert(hintId, 'The blocked input reason has an accessible description ID');
    const blockedComposer = task.getByRole('textbox', '补充给 整理本月运营情况');
    await expect(blockedComposer).toHaveAttribute('placeholder', '发消息');
    await expect(blockedComposer).toHaveAttribute('aria-describedby', hintId);
    const description = await browser.evaluate(id => {
      const hint = document.getElementById(id);
      return { text: hint?.textContent ?? null, label: hint?.closest('form')?.querySelector('textarea')?.getAttribute('aria-label') ?? null, count: [...document.querySelectorAll('[id]')].filter(element => element.id === id).length };
    }, hintId);
    assert.deepEqual(description, { text: blockedReason, label: '补充给 整理本月运营情况', count: 1 });
    await app.screenshot('chat-confidence-unknown-phone');
  } finally { db.close(); }
});
