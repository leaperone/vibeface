import assert from 'node:assert/strict';
import { join } from 'node:path';
import { DatabaseSync } from 'node:sqlite';
import { stat } from 'node:fs/promises';
import { test } from './fixtures';
import { expect, secrets } from 'e2e';

test('creates a persistent Bot without a model and shows triage budget state', async ({ app, node, screen, browser }) => {
  await app.open(node.url);
  await screen.getByRole('button', '新建我的网络').click();
  await screen.getByLabel('这台电脑叫什么？').fill('E2E Node');
  await screen.getByRole('button', '下一步').click();
  await expect(screen.getByRole('heading', '设置恢复密码')).toBeVisible();
  await screen.getByLabel(/^恢复密码/).fill(secrets.get('recoveryPassword'));
  await screen.getByRole('button', '下一步').click();
  await expect(screen.getByRole('heading', '保存恢复文件')).toBeVisible();
  const download = await browser.waitForDownload(() => screen.getByRole('button', '下载恢复文件').click());
  expect((await stat(download.absolutePath)).size).toBeGreaterThan(0);
  await screen.getByRole('checkbox', '已把文件和密码分开保存').check();
  await screen.getByRole('button', '完成创建').click();
  await expect(screen.getByRole('heading', '让这台电脑运行 Bot')).toBeVisible();
  await screen.getByRole('button', '稍后设置').click();
  await screen.getByLabel('名称', { exact: true }).fill('E2E Bot');
  await screen.getByRole('button', '完成').click();
  await expect(browser).toHaveURL(/\/b\/[^/?#]+$/);
  const bot = screen.getByRole('region', '对话').getByRole('heading', 'E2E Bot');
  await expect(bot).toBeVisible();
  await expect(screen.getByText('Bot 还不能回复')).toBeVisible();
  await browser.reload();
  await expect(bot).toBeVisible();
  await app.screenshot('onboarding-complete');

  await browser.goto(`${node.url}/settings`);
  await screen.getByRole('button', /^转发到外部聊天/).click();
  const triage = screen.getByRole('status', '分诊状态');
  await expect(triage).toContainText('分诊今日已用 0 / 20 次');
  await expect(triage).toContainText('待审阅 0 · 审阅中 0 · 待确认 0');
  await expect(triage).not.toContainText('分诊暂停');

  const database = new DatabaseSync(join(node.directory, 'node.sqlite3'));
  try {
    const network = database.prepare('SELECT network_id FROM networks').get();
    assert(typeof network?.network_id === 'string');
    database.prepare("INSERT INTO budget_days(network_id,day,used) VALUES(?,date('now'),20)")
      .run(`triage:${network.network_id}`);
    const insert = database.prepare('INSERT INTO triage_queue(ts_ms,event_id,event) VALUES(?,?,?)');
    for (const [index, id] of ['1', '2', '3'].entries()) {
      const event_id = id.repeat(32), ts_ms = Date.now() + index;
      const event = { schema_version: 1, event_id, source: 'hook', node: node.peer, agent: 'codex',
        event: 'turn_end', session_id: 'budget-ui-fixture', turn_id: event_id, ts_ms,
        ...(index === 2 ? { _triage: { attempt: '4'.repeat(32), boot: '5'.repeat(32), state: 'unknown' } } : {}) };
      insert.run(ts_ms, event_id, JSON.stringify(event));
    }
  } finally { database.close(); }
  await browser.reload();
  await screen.getByRole('button', /^转发到外部聊天/).click();
  await expect(triage).toContainText('分诊今日已用 20 / 20 次');
  await expect(triage).toContainText('今日额度已用完，分诊暂停');
  await expect(triage).toContainText('后恢复。');
  await expect(triage).toContainText('待审阅 2 · 审阅中 0 · 待确认 1');
  await expect(triage).toContainText('结果待确认的事件不会自动重试。');
  await app.screenshot('triage-budget-paused');

  const decisionDb = new DatabaseSync(join(node.directory, 'node.sqlite3'));
  try {
    const identity = decisionDb.prepare('SELECT network_id, conversation_id FROM bots WHERE name=?').get('E2E Bot');
    assert(typeof identity?.network_id === 'string' && typeof identity.conversation_id === 'string');
    const insertQuestion = decisionDb.prepare('INSERT INTO questions(network_id,question_id,conversation_id,rank,author,created_at_ms,node,session_id,body) VALUES(?,?,?,?,?,?,?,?,?)');
    const readAnswer = decisionDb.prepare('SELECT text FROM messages WHERE network_id=? AND answers=?');
    const seed = (id: string, choices: string[]) => {
      const body = { type: 'question_asked', question_id: id, severity: 'attention', text: '报告用哪种格式交付？\nMarkdown 可继续编辑，PDF 保留排版。\n对方尚未指定格式，需要你决定。', choices: choices.map(label => ({ label })), about: { node: node.peer, agent: 'codex', session_id: id, cwd: 'workspace', summary: '' } };
      insertQuestion.run(identity.network_id, id, identity.conversation_id, id, node.peer, Date.now(), node.peer, id, JSON.stringify(body));
    };
    const three = ['导出 Markdown，保留可编辑内容', '导出 PDF，保留当前排版', '同时导出两种格式'];
    seed('q-decision-three', three);
    await browser.goto(`${node.url}/inbox`);
    await expect(screen.getByRole('heading', '报告用哪种格式交付？')).toBeVisible();
    await expect(screen.getByRole('radio')).toHaveCount(4);
    await expect(screen.getByLabel('你的回答')).toHaveCount(0);
    await screen.getByRole('radio', three[0]).check();
    await screen.getByRole('radio', three[1]).check();
    assert.equal(readAnswer.get(identity.network_id, 'q-decision-three'), undefined);
    await expect(screen.getByRole('radio', three[1])).toBeChecked();
    await app.screenshot('decision-three-selected-desktop');
    await screen.getByRole('button', '发送回答', { exact: true }).click();
    await expect(screen.getByRole('radio')).toHaveCount(0);
    await expect(screen.getByRole('heading', '报告用哪种格式交付？')).toHaveCount(0);
    assert.equal(readAnswer.get(identity.network_id, 'q-decision-three')?.text, three[1]);

    seed('q-decision-four', [...three, '先发送摘要，正文保留可编辑版本']);
    await browser.goto(`${node.url}/tasks`);
    await expect(screen.getByRole('link', '待处理 1 项', { exact: true })).toBeVisible();
    await expect(screen.getByRole('radiogroup', '任务状态').getByRole('radio')).toHaveCount(4);
    await expect(screen.getByRole('heading', '报告用哪种格式交付？')).toHaveCount(0);
    await expect(screen.getByRole('button', '发送回答', { exact: true })).toHaveCount(0);
    await expect(screen.getByLabel('你的回答')).toHaveCount(0);
    await browser.goto(`${node.url}/inbox`);
    await browser.setViewport({ width: 375, height: 812 });
    await expect(screen.getByRole('radio')).toHaveCount(5);
    await screen.getByRole('radio', '自己写回答').check();
    await screen.getByLabel('你的回答').fill('1');
    await screen.getByLabel('你的回答').press('Enter');
    await expect(screen.getByLabel('你的回答')).toHaveValue('1\n');
    await screen.getByLabel('你的回答').fill('1\n１');
    await expect(screen.getByLabel('你的回答')).toHaveValue('1\n１');
    assert.equal(readAnswer.get(identity.network_id, 'q-decision-four'), undefined);
    await app.screenshot('decision-four-custom-narrow');
    await screen.getByRole('button', '发送回答', { exact: true }).click();
    await expect(screen.getByRole('radio')).toHaveCount(0);
    await expect(screen.getByRole('heading', '报告用哪种格式交付？')).toHaveCount(0);
    assert.equal(readAnswer.get(identity.network_id, 'q-decision-four')?.text, '1\n１');

    seed('q-decision-text', []);
    await browser.reload();
    await expect(screen.getByLabel('你的回答')).toBeVisible();
    await screen.getByLabel('你的回答').fill('先说明收件人是否能编辑，再确认交付格式。');
    await screen.getByRole('button', '发送回答', { exact: true }).click();
    await expect(screen.getByLabel('你的回答')).toHaveCount(0);
    await expect(screen.getByRole('heading', '报告用哪种格式交付？')).toHaveCount(0);
    assert.equal(readAnswer.get(identity.network_id, 'q-decision-text')?.text, '先说明收件人是否能编辑，再确认交付格式。');
  } finally { decisionDb.close(); }
});
