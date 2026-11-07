import assert from 'node:assert/strict';
import { join } from 'node:path';
import { DatabaseSync } from 'node:sqlite';
import { test } from './fixtures';
import { expect, secrets } from 'e2e';

test('chat keeps decisions and task records readable on desktop and phone', async ({ app, node, screen, browser }) => {
  await browser.setViewport({ width: 1280, height: 900 });
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
    const summary = '已汇总现有记录。\n正在检查退款是否单独统计。\n详细资料保留在这里，不需要默认展开。';
    const message = db.prepare('INSERT INTO messages(network_id,conversation_id,message_id,event_hash,rank,author,role,text,created_at_ms,in_reply_to,status,attachments) VALUES(?,?,?,?,?,?,?,?,?,?,?,?)');
    let rank = 0;
    const addMessage = (id: string, text: string, replyTo?: string) => message.run(network, conversation, id, `fixture-${id}`, String(++rank).padStart(20, '0'), node.peer, replyTo ? 'assistant' : 'user', text, Date.now() - 60000 + rank, replyTo ?? null, replyTo ? 'completed' : null, '[]');
    for (const [id, text] of [['report', '请整理本月运营情况'], ['scroll-request', '核对即将完成的记录'], ['compact-request', '保留已完成任务入口']]) {
      addMessage(id, text);
      addMessage(`${id}-reply`, `已收到，${text}`, id);
    }
    for (let index = 1; index <= 20; index++) {
      addMessage(`history-user-${index}`, `后续记录 ${index}，继续检查运营资料，保留尚未确认的内容。`);
      addMessage(`history-reply-${index}`, `第 ${index} 份资料已记录，后续结果会单独报告。`, `history-user-${index}`);
    }
    const addWork = (id: string, trigger: string, title: string, outcome?: 'completed' | 'unverified', thread?: string) => {
      const dispatch = { type: 'work_dispatched', work_id: id, trigger, dispatcher: node.peer, call_ref: id, kind: 'task', resolved: { provider: 'codex', model: null }, cwd: 'workspace', title, prompt: title };
      const progress = { type: 'work_progress', work_id: id, phase: 'running', summary };
      const finish = outcome ? { type: 'work_finished', work_id: id, outcome, result_text: outcome === 'completed' ? `完整结果 ${id}` : '', failure_reason: outcome === 'unverified' ? '这次的结果无法确认。' : null } : null;
      db.prepare('INSERT INTO works(network_id,work_id,conversation_id,target_node,dispatch,dispatch_rank,progress,progress_rank,thread_id,finish,finish_rank) VALUES(?,?,?,?,?,?,?,?,?,?,?)').run(network, id, conversation, node.peer, JSON.stringify(dispatch), String(++rank), JSON.stringify(progress), String(++rank), thread ?? null, finish ? JSON.stringify(finish) : null, finish ? String(++rank) : null);
    };
    addWork(work, 'message:report', '整理本月运营情况');
    addWork('chat-confidence-sibling', 'message:report', '同次请求的已完成任务', 'completed');
    addWork('chat-confidence-child', `task:${work}`, '嵌套任务', 'completed');
    addWork('chat-confidence-approval', 'message:report', '等待允许的任务', undefined, 'approval-session');
    addWork('chat-confidence-unknown', 'message:report', '结果未知的任务', 'unverified');
    addWork('chat-confidence-scrolling', 'message:scroll-request', '即将完成的任务');
    addWork('chat-confidence-compact', 'message:compact-request', '完成行不重复的标题', 'completed');
    for (let index = 1; index <= 12; index++) addWork(`chat-confidence-orphan-${index}`, `message:missing-${index}`, `历史任务 ${index}`, 'completed');
    const question = { type: 'question_asked', question_id: 'report-scope', severity: 'attention', text: '退款需要单独统计吗？\n选择后会按这个口径整理。', choices: [{ label: '单独统计' }, { label: '合并统计' }], about: { node: node.peer, agent: null, session_id: 'report', cwd: 'workspace', summary: '' } };
    db.prepare('INSERT INTO questions(network_id,question_id,conversation_id,rank,author,created_at_ms,node,session_id,body) VALUES(?,?,?,?,?,?,?,?,?)').run(network, question.question_id, conversation, '3', node.peer, Date.now(), node.peer, 'report', JSON.stringify(question));
    const approval = { ...question, question_id: 'report-approval', text: '允许写入运营汇总吗？', choices: [{ label: '允许' }, { label: '拒绝' }], about: { ...question.about, agent: 'codex', session_id: 'approval-session', request_id: 'fixture-request' } };
    db.prepare('INSERT INTO questions(network_id,question_id,conversation_id,rank,author,created_at_ms,node,session_id,request_id,body) VALUES(?,?,?,?,?,?,?,?,?,?)').run(network, approval.question_id, conversation, '4', node.peer, Date.now() + 1, node.peer, 'approval-session', 'fixture-request', JSON.stringify(approval));
    await browser.reload();
    await expect(screen.getByRole('heading', '退款需要单独统计吗？')).toBeVisible();
    await expect(screen.getByText('选择后会按这个口径整理。', { exact: true })).toBeVisible();
    await expect(screen.getByRole('radio', '单独统计', { exact: true })).toBeVisible();
    await expect(screen.getByRole('heading', '允许写入运营汇总吗？')).toBeVisible();
    await expect(screen.getByRole('heading', '待确认 · 结果未知的任务')).toBeVisible();
    await app.screenshot('chat-confidence-decision-desktop');

    const chat = screen.getByRole('region', '对话');
    const tasksButton = () => screen.getByRole('button', /^任务(?: ·|$)/);
    const group = browser.locator('[data-message-id="task-group:message:report"]');
    const groupLabel = '4 个任务 · 1 进行中 · 2 待处理';
    const order = await browser.evaluate(() => [...document.querySelectorAll('[data-message-id]')].map(item => item.getAttribute('data-message-id')));
    assert(order.indexOf('report') < order.indexOf('task-group:message:report') && order.indexOf('task-group:message:report') < order.indexOf('report-reply'), 'tasks stay between the originating request and its reply');
    assert(order.includes(`task-group:task:${work}`), 'nested triggers resolve to the visible request');
    await expect(chat.getByRole('link', /^历史任务 /)).toHaveCount(0);
    const compact = browser.locator('[data-message-id="task-group:message:compact-request"]');
    await compact.scrollIntoView();
    await expect(compact).toHaveText('已完成 · 查看任务');
    await group.scrollIntoView();
    await expect(group.getByText(groupLabel, { exact: true })).toBeVisible();
    assert.equal(await browser.evaluate(() => document.querySelector('[data-message-id="task-group:message:report"] details')?.hasAttribute('open') ?? null), false);
    await group.getByText(groupLabel, { exact: true }).click();
    await expect(group.getByRole('link', '整理本月运营情况 · 运行中 · 查看任务')).toBeVisible();

    await tasksButton().click();
    let history = screen.getByRole('dialog', '任务记录');
    await expect(history.getByRole('link')).toHaveCount(19);
    const wideGeometry = await browser.evaluate(() => {
      const sheet = document.querySelector('[data-slot="sheet-content"]');
      if (!sheet) throw new Error('Task history is open');
      return { width: sheet.clientWidth, content: sheet.scrollWidth, page: document.documentElement.clientWidth, pageContent: document.documentElement.scrollWidth };
    });
    assert(wideGeometry.content <= wideGeometry.width && wideGeometry.pageContent <= wideGeometry.page, 'desktop task history must not scroll horizontally');
    await history.getByRole('link', /历史任务 12 /).scrollIntoView();
    await expect(history.getByRole('link', /历史任务 12 /)).toBeVisible();
    await app.screenshot('chat-confidence-history-desktop');
    await browser.keyboard.press('Escape');
    await expect(history).not.toBeVisible();
    await expect(browser.locator('button[aria-label^="任务"]:focus')).toBeVisible();
    await tasksButton().click();
    history = screen.getByRole('dialog', '任务记录');
    await history.getByRole('link', /历史任务 12 /).click();
    await expect(browser).toHaveURL(new RegExp(`/b/${bot}/work/chat-confidence-orphan-12$`));
    await expect(screen.getByRole('dialog', '任务记录')).not.toBeVisible();
    await expect(tasksButton()).toHaveCount(0);
    await expect(browser.locator('aside[aria-label="任务详情"]:focus')).toBeVisible();
    await screen.getByRole('complementary', '任务详情').getByRole('link', '关闭', { exact: true }).click();
    await expect(browser.locator('#main:focus')).toBeVisible();
    await expect(browser).toHaveURL(new RegExp(`/b/${bot}$`));

    await browser.locator('[data-message-id="history-user-10"]').scrollIntoView();
    const viewport = await browser.evaluate(() => {
      const item = document.querySelector('[data-message-id="history-user-10"]');
      if (!item) throw new Error('The reading anchor exists');
      const rect = item.getBoundingClientRect();
      return { x: rect.left + 20, y: rect.top + 20 };
    });
    await browser.mouse.move(viewport.x, viewport.y);
    await browser.mouse.wheel(0, -40);
    await expect(screen.getByRole('button', '回到最新消息')).toBeVisible();
    const readingPosition = () => browser.evaluate(async () => {
      const item = document.querySelector('[data-message-id="history-user-10"]');
      const viewport = document.querySelector('[data-slot="message-scroller-viewport"]');
      if (!item || !(viewport instanceof HTMLElement)) throw new Error('The reading anchor and viewport exist');
      const read = () => ({ top: item.getBoundingClientRect().top, scrollTop: viewport.scrollTop, scrollHeight: viewport.scrollHeight, height: viewport.clientHeight, animating: viewport.hasAttribute('data-autoscrolling') });
      let previous = read(), stable = 0;
      for (let frame = 0; frame < 120; frame++) {
        await new Promise<void>(resolve => requestAnimationFrame(() => resolve()));
        const current = read();
        stable = !current.animating && Math.abs(current.top - previous.top) < 0.1 && Math.abs(current.scrollTop - previous.scrollTop) < 0.1 && current.scrollHeight === previous.scrollHeight && current.height === previous.height ? stable + 1 : 0;
        if (stable >= 3) return current;
        previous = current;
      }
      throw new Error(`Chat reading position did not settle: ${JSON.stringify(previous)}`);
    });
    const before = await readingPosition();
    const completed = { type: 'work_finished', work_id: 'chat-confidence-scrolling', outcome: 'completed', result_text: '核对完成', failure_reason: null };
    db.prepare('UPDATE works SET finish=?,finish_rank=? WHERE network_id=? AND work_id=?').run(JSON.stringify(completed), '999', network, 'chat-confidence-scrolling');
    db.prepare('UPDATE works SET progress=?,progress_rank=? WHERE network_id=? AND work_id=?').run(JSON.stringify({ type: 'work_progress', work_id: work, phase: 'running', summary: `${summary}\n已收到新的资料。` }), '1000', network, work);
    await browser.evaluate(() => document.dispatchEvent(new Event('visibilitychange')));
    await expect(browser.locator('[data-message-id="task-group:message:scroll-request"] a[aria-label="即将完成的任务 · 已完成 · 查看任务"]')).toBeAttached();
    assert.equal(await browser.evaluate(() => document.querySelector('[data-message-id="task-group:message:report"] details')?.hasAttribute('open') ?? null), true, 'progress updates preserve the expanded group');
    const after = await readingPosition();
    assert(Math.abs(after.top - before.top) < 3, `completion preserves the message being read: ${JSON.stringify({ before, after })}`);

    await screen.getByRole('link', '我的设备', { exact: true }).click();
    await expect(tasksButton()).toHaveCount(0);
    await screen.getByRole('link', /^运营助手(?:\s|$)/).click();
    await tasksButton().click();
    await browser.back();
    await expect(browser).toHaveURL(`${node.url}/devices`);
    await expect(screen.getByRole('dialog', '任务记录')).toHaveCount(0);
    await expect(tasksButton()).toHaveCount(0);
    await screen.getByRole('link', /^运营助手(?:\s|$)/).click();
    await screen.getByRole('button', '新 Bot', { exact: true }).click();
    await screen.getByLabel('名称', { exact: true }).fill('另一个助手');
    await screen.getByRole('button', '创建', { exact: true }).click();
    await expect(browser).not.toHaveURL(new RegExp(`/b/${bot}$`));
    await screen.getByRole('link', /^运营助手(?:\s|$)/).click();
    await tasksButton().click();
    await browser.back();
    await expect(screen.getByRole('dialog', '任务记录')).toHaveCount(0);
    await tasksButton().click();
    await expect(screen.getByRole('dialog', '任务记录').getByText('还没有任务', { exact: true })).toBeVisible();
    await browser.keyboard.press('Escape');
    await screen.getByRole('link', /^运营助手(?:\s|$)/).click();

    await browser.goto(`${node.url}/b/${bot}/work/${work}`);
    const desktopTask = screen.getByRole('complementary', '任务详情');
    await expect(desktopTask.getByText('详细进展', { exact: true })).toBeVisible();
    const detailOpen = () => browser.evaluate(() => [...document.querySelectorAll('details')].find(item => item.querySelector('summary')?.textContent?.includes('详细进展'))?.open ?? null);
    assert.equal(await detailOpen(), false);
    await desktopTask.getByText('详细进展', { exact: true }).click();
    assert.equal(await detailOpen(), true);
    await expect(desktopTask.getByText(`${summary}\n已收到新的资料。`, { exact: true })).toBeVisible();
    await app.screenshot('chat-confidence-progress-desktop');

    const finish = { type: 'work_finished', work_id: work, outcome: 'unverified', result_text: '', failure_reason: '这次的结果无法确认。' };
    db.prepare('UPDATE works SET finish=?,finish_rank=? WHERE network_id=? AND work_id=?').run(JSON.stringify(finish), '1001', network, work);
    await browser.setViewport({ width: 390, height: 844 });
    await browser.goto(`${node.url}/b/${bot}`);
    await tasksButton().click();
    history = screen.getByRole('dialog', '任务记录');
    await expect(history.getByRole('link')).toHaveCount(19);
    await history.getByRole('link', /历史任务 12 /).scrollIntoView();
    await expect(history.getByRole('link', /历史任务 12 /)).toBeVisible();
    const sheetGeometry = await browser.evaluate(() => {
      const sheet = document.querySelector('[data-slot="sheet-content"]');
      if (!sheet) throw new Error('Task history is open');
      return { width: sheet.clientWidth, content: sheet.scrollWidth, page: document.documentElement.clientWidth, pageContent: document.documentElement.scrollWidth };
    });
    assert(sheetGeometry.content <= sheetGeometry.width && sheetGeometry.pageContent <= sheetGeometry.page, 'phone task history must not scroll horizontally');
    await app.screenshot('chat-confidence-history-phone');
    await history.getByRole('link', /^整理本月运营情况 /).click();
    await expect(screen.getByRole('dialog', '任务记录')).not.toBeVisible();
    const task = screen.getByRole('dialog', '整理本月运营情况');
    await expect(task.getByRole('heading', '还不知道做完没有')).toBeVisible();
    await expect(task.getByRole('button', '先查结果', { exact: true })).toBeVisible();
    await expect(task.getByRole('button', '做完了', { exact: true })).not.toBeVisible();
    await task.getByText('我已自行核对', { exact: true }).click();
    await expect(task.getByRole('button', '做完了', { exact: true })).toBeVisible();
    await expect(task.getByRole('button', '没做完', { exact: true })).toBeVisible();
    const geometry = await browser.evaluate(() => ({ width: document.documentElement.clientWidth, content: document.documentElement.scrollWidth }));
    assert(geometry.content <= geometry.width, 'phone layout must not scroll horizontally');
    await app.screenshot('chat-confidence-unknown-phone');
  } finally { db.close(); }
});
