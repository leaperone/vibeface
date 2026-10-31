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

  const contextDb = new DatabaseSync(join(node.directory, 'node.sqlite3'));
  const viewport = await browser.evaluate(() => ({ width: window.innerWidth, height: window.innerHeight }));
  try {
    const identity = contextDb.prepare('SELECT network_id, conversation_id FROM bots WHERE name=?').get('E2E Bot');
    assert(typeof identity?.network_id === 'string' && typeof identity.conversation_id === 'string');
    const shown = { bot: null, works: {}, session_generation: 4, context: {
      attempt_id: 'p7-public-projection-fixture', provider: 'codex', stage: 'compiled', session_generation: 4,
      stable_fingerprint: 'a'.repeat(64), dynamic_fingerprint: 'b'.repeat(64), stable_bytes: 100, dynamic_bytes: 200, invalidation_reason: null,
      coverage: { messages_omitted: 1, works_omitted: 0, dispatches_omitted: 0, questions_omitted: 0, attachments_omitted: 0, memory_omitted: 3,
        checkpoint_omitted: 1, handoff_omitted: 2, pending_checks_omitted: 3, bot_indexes_omitted: { skills: 4 }, history_retrieval: 'context' },
    }, memory_conflicts: [{ call_ref: 'p7-public-memory-fixture', memory_id: 'p7-ui-memory', expected_epoch: 12, actual_epoch: 12,
      expected_revision: 'b'.repeat(64), actual_revision: 'c'.repeat(64), reason: 'revision_changed' }] };
    const saveContext = contextDb.prepare("INSERT INTO scheduler_sessions(conversation_id,scope,next_sequence,covers,owner_epoch,permission_version,shown) VALUES(?,'main',1,'{}',0,1,?) ON CONFLICT(conversation_id,scope) DO UPDATE SET shown=excluded.shown");
    assert.equal(saveContext.run(identity.conversation_id, JSON.stringify(shown)).changes, 1);
    const showContext = async () => {
      await browser.reload();
      await expect(bot).toBeVisible();
      await screen.getByRole('heading', 'E2E Bot', { level: 1 }).getByRole('button', 'E2E Bot', { exact: true }).click();
      const dialog = screen.getByRole('dialog');
      await dialog.getByRole('button', /^上下文和会话/).click();
      await expect(dialog.getByRole('heading', '上下文和会话')).toBeVisible();
      return dialog;
    };
    await browser.setViewport({ width: 375, height: 812 });
    let dialog = await showContext();
    await expect(dialog.getByText('已编译', { exact: true })).toBeVisible();
    await expect(dialog.getByText('内容已准备，尚未确认 AI 开始执行。')).toBeVisible();
    await expect(dialog.getByText('当前续接状态未知。')).toBeVisible();
    await expect(dialog.getByText('可尝试续接', { exact: true })).toHaveCount(0);
    await expect(dialog.getByText('下轮重建', { exact: true })).toHaveCount(0);
    await dialog.getByText('本轮上下文详情', { exact: true }).click();
    await expect(dialog.getByText('会话代次 4 · 已编译 · Codex')).toBeVisible();
    await expect(dialog.getByText('历史检查点 · 1')).toBeVisible();
    await expect(dialog.getByText('交接摘要 · 2')).toBeVisible();
    await expect(dialog.getByText('结果待确认 · 3')).toBeVisible();
    await expect(dialog.getByText('记忆 · 3')).toBeVisible();
    await expect(dialog.getByText('Skill · 4')).toBeVisible();
    await app.screenshot('context-fixture-compiled-coverage-narrow');
    await dialog.getByRole('button', '新增记忆', { exact: true }).click();
    await dialog.getByLabel('记忆内容', { exact: true }).fill('浏览器签名记忆，回复使用中文。');
    await dialog.getByRole('button', '保存记忆', { exact: true }).click();
    await expect(dialog.getByText('查看已保存记忆 · 1', { exact: true })).toBeVisible();
    await dialog.getByText('查看已保存记忆 · 1', { exact: true }).click();
    await expect(dialog.getByText('浏览器签名记忆，回复使用中文。', { exact: true })).toBeVisible();
    const memoryRow = () => contextDb.prepare('SELECT memory_id,text,epoch,revision,control_revision,origin,user_controlled,essential,source_event_hash FROM memories WHERE network_id=? AND conversation_id=?').get(identity.network_id, identity.conversation_id);
    const created = memoryRow();
    assert(created && typeof created.memory_id === 'string' && typeof created.revision === 'string' && typeof created.source_event_hash === 'string');
    assert.equal(created.epoch, 12);
    assert.equal(created.control_revision, 1);
    assert.equal(created.origin, JSON.stringify('user'));
    assert.equal(created.user_controlled, 1);
    assert.equal(created.essential, 1);
    const stored = contextDb.prepare('SELECT envelope FROM events WHERE network_id=? AND hash=?').get(identity.network_id, created.source_event_hash);
    assert(typeof stored?.envelope === 'string');
    const userEvent = JSON.parse(JSON.parse(stored.envelope).canonical_body);
    assert.equal(userEvent.body.type, 'memory_controlled');
    assert.equal(userEvent.body.memory_id, created.memory_id);
    assert.equal(userEvent.body.base.version, null);
    assert.equal(typeof userEvent.body.attestation.signature, 'string');
    await expect(dialog.getByText('你记录 · 由你维护，受保护 · 优先带入上下文')).toBeVisible();
    await dialog.getByText('记忆版本', { exact: true }).click();
    await expect(dialog.getByText(`数据版本 12 · 维护版本 1 · ${created.revision}`)).toBeVisible();
    await dialog.getByText('查看记忆冲突 · 1', { exact: true }).click();
    await expect(dialog.getByText('记忆已被更新，未覆盖新值')).toBeVisible();
    await app.screenshot('context-signed-memory-narrow');
    await dialog.getByRole('button', '编辑记忆', { exact: true }).click();
    await dialog.getByLabel('记忆内容', { exact: true }).fill('浏览器已编辑的签名记忆。');
    await dialog.getByRole('button', '保存记忆', { exact: true }).click();
    await expect(dialog.getByText('浏览器已编辑的签名记忆。', { exact: true })).toBeVisible();
    const edited = memoryRow();
    assert.equal(edited?.control_revision, 2);
    assert.notEqual(edited?.revision, created.revision);
    await app.screenshot('context-signed-memory-edited-narrow');
    await dialog.getByRole('button', '编辑记忆', { exact: true }).click();
    await dialog.getByRole('button', '删除记忆', { exact: true }).click();
    await dialog.getByRole('button', '确认删除记忆', { exact: true }).click();
    await expect(dialog.getByText('暂无已保存记忆', { exact: true })).toBeVisible();
    const deleted = memoryRow();
    assert.equal(deleted?.text, null);
    assert.equal(deleted?.control_revision, 3);
    assert.equal(deleted?.user_controlled, 1);
    assert.equal(deleted?.memory_id, created.memory_id);
    assert.equal(contextDb.prepare("SELECT COUNT(*) AS n FROM events WHERE network_id=? AND scope_id=? AND kind='memory_controlled'").get(identity.network_id, identity.conversation_id)?.n, 3);
    await app.screenshot('context-signed-memory-deleted-narrow');
    shown.context.stage = 'unknown';
    assert.equal(saveContext.run(identity.conversation_id, JSON.stringify(shown)).changes, 1);
    dialog = await showContext();
    await expect(dialog.getByText('结果未知', { exact: true })).toBeVisible();
    await expect(dialog.getByText('先查明执行结果，请勿直接重试。')).toBeVisible();
    await expect(dialog.getByText('已发送', { exact: true })).toHaveCount(0);
  } finally {
    contextDb.close();
    await browser.setViewport(viewport);
  }

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
    await expect(screen.getByRole('main').getByRole('link', '待处理 2', { exact: true })).toBeVisible();
    await expect(screen.getByRole('radiogroup', '任务状态').getByRole('radio')).toHaveCount(4);
    await expect(screen.getByRole('heading', '报告用哪种格式交付？')).toHaveCount(1);
    await expect(screen.getByRole('button', '发送回答', { exact: true })).toHaveCount(1);
    await expect(screen.getByText('问题待确认', { exact: true })).toHaveCount(0);
    await expect(screen.getByText('2 项任务', { exact: true })).toBeVisible();
    await expect(screen.getByLabel('你的回答')).toHaveCount(0);
    await app.screenshot('task-overview-single-question-and-turn');
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
