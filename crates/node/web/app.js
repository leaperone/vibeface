import { Identity, recoverySeal, recoveryOpen } from "/generated/vibeface_secure.js";
import { initClient, connectDevice, readInvitation, validEndpoint, validTarget, signClient } from "/transport.js";

const $ = (id) => document.getElementById(id);
const storage = {
  get(key, fallback) { try { return JSON.parse(localStorage.getItem(`vibeface.${key}`)) ?? fallback; } catch { return fallback; } },
  set(key, value, required = false) { try { localStorage.setItem(`vibeface.${key}`, JSON.stringify(value)); } catch { if (required) throw new Error("浏览器无法保存设备与操作记录。请允许此站点使用本地存储后重试。"); } },
};
const newId = () => { if (!globalThis.crypto?.getRandomValues) throw new Error("浏览器不支持安全的操作编号。请使用可信 HTTPS 客户端或 localhost。"); return globalThis.crypto.randomUUID?.() || [...globalThis.crypto.getRandomValues(new Uint8Array(16))].map((byte) => byte.toString(16).padStart(2, "0")).join(""); };
let clientId = "";
const initialFragment = new URLSearchParams(location.hash.slice(1));
const initialInvitation = initialFragment.has("bootstrap") ? { bootstrap: true, value: initialFragment.get("bootstrap") } : initialFragment.has("invite") ? { bootstrap: false, value: initialFragment.get("invite") } : null;
if (location.hash) { const clean = new URL(location.href); clean.hash = ""; history.replaceState(null, "", clean); }
const state = { member: null, info: {}, client: null, channel: null, entry: null, pair: null, view: "chat", nodes: [], conversations: [], conversationId: new URL(location.href).searchParams.get("conversation") || "", data: null, notifications: [], actual: new Map(), pending: [], pendingMessages: new Map(), drafts: new Map(), refreshing: false, refreshAgain: false, timer: null, renderKey: "", renderedMessages: [], renderedConversation: "", authEpoch: 0 };
const roleNames = { gateway: "接入", relay: "中继", replica: "同步", scheduler: "调度", executor: "执行" };
const stateNames = { accepted: "已接收", running: "执行中", unknown: "结果未知", succeeded: "操作完成", failed: "失败", rejected: "已拒绝", open: "已打开", opening: "正在打开", working: "处理中", needs_review: "待验收", idle: "空闲", pending: "待处理", completed: "已完成", awaiting_acceptance: "待验收", cancelled: "已取消", blocked: "受阻", ready: "就绪" };
const nodeName = (id) => id ? `${id.slice(0, 10)}${id.length > 10 ? "…" : ""}` : "未知设备";
const textOf = (value) => typeof value === "string" ? value : JSON.stringify(value, null, 2);
const el = (tag, className, text) => { const element = document.createElement(tag); if (className) element.className = className; if (text !== undefined) element.textContent = text; return element; };
function showError(error) { $("error").textContent = error instanceof Error ? error.message : String(error); $("error").hidden = false; }
function notice(text) { $("notice").textContent = text; }
function clearError() { $("error").hidden = true; $("error").textContent = ""; }
function connection(online, text) { document.querySelector(".topbar").dataset.connection = online ? "online" : "offline"; $("connection-state").textContent = text; }
function empty(title, detail) { const item = el("div", "empty"); item.append(el("strong", "", title), el("p", "", detail)); return item; }
function badge(value, label = stateNames[value] || value || "未知") { return el("span", `badge ${/^[a-z_]+$/.test(value || "") ? value : ""}`, label); }
function button(text, action, className = "secondary") { const item = el("button", className, text); item.type = "button"; item.addEventListener("click", () => perform(item, action)); return item; }
function field(form, labelText, name, type = "text", value = "") { const label = el("label", "", labelText); const input = el(type === "textarea" ? "textarea" : "input"); input.name = name; if (type !== "textarea") input.type = type; else input.rows = 3; input.required = true; input.value = value; label.append(input); form.append(label); return input; }
function detailList(pairs) { const list = el("dl"); for (const [key, value] of pairs) list.append(el("dt", "", key), el("dd", "", value ?? "未提供")); return list; }
function rawDetails(title, value) { const item = el("details"); item.append(el("summary", "", title), el("pre", "", textOf(value))); return item; }
function editable(container) { return container.contains(document.activeElement) && /^(INPUT|TEXTAREA|SELECT)$/.test(document.activeElement.tagName); }
async function perform(control, action) {
  if (control.disabled) return;
  clearError(); control.disabled = true; control.setAttribute("aria-busy", "true");
  try { await action(); } catch (error) { showError(error); }
  finally { control.disabled = false; control.removeAttribute("aria-busy"); updateComposer(); }
}
function bindForm(id, action) { $(id).addEventListener("submit", (event) => { event.preventDefault(); const form = event.currentTarget; if (form.reportValidity()) perform(form.querySelector('[type="submit"]'), () => action(form)); }); }
async function request(method, params = {}) {
  if (!state.channel) throw new Error("先连接并核对自己的设备。");
  const epoch = state.authEpoch;
  const data = await state.channel.request(method, params);
  if (epoch !== state.authEpoch) throw changedConnection();
  return data;
}
const rpc = (method, params = {}) => request("rpc", { method, ...params });
function networkKey(key) { return `${key}.${state.member?.network_id || "unpaired"}`; }
function frontier() { return storage.get(networkKey("frontiers"), {})[state.conversationId] || []; }
function incomplete() { const missing = state.data?.missing; return Boolean(missing && ["events", "blobs", "conflicts"].some((key) => Array.isArray(missing[key]) ? missing[key].length : missing[key])); }
function readEntry(form) { return { address: validEndpoint(form.elements.address.value.trim()), node_id: validTarget(form.elements.node_id.value.trim()), relay: form.elements.relay?.checked || false }; }
function entryKey(entry) { return `${entry.address}:${entry.node_id}:${Boolean(entry.relay)}`; }
function savedEntries() { return storage.get("connections", []).filter((entry) => { try { return validEndpoint(entry.address) === entry.address && validTarget(entry.node_id); } catch { return false; } }); }
function saveEntry(entry) { storage.set("connections", [...savedEntries().filter((item) => entryKey(item) !== entryKey(entry)), entry], true); renderEntries(); }
function renderEntries() {
  $("entry-list").replaceChildren(); $("saved-connections").replaceChildren();
  for (const entry of savedEntries()) {
    const title = `${entry.address.replace(/^https?:\/\//, "")} → ${nodeName(entry.node_id)}${entry.relay ? " · 中继" : ""}`;
    const li = el("li"); const control = button(title, () => openEntry(entry), "quiet");
    if (state.entry && entryKey(entry) === entryKey(state.entry)) control.setAttribute("aria-current", "location");
    li.append(control, button("移除", () => { storage.set("connections", savedEntries().filter((item) => entryKey(item) !== entryKey(entry))); renderEntries(); }, "quiet"));
    $("entry-list").append(li); $("saved-connections").append(button(`连接 ${title}`, () => openEntry(entry)));
  }
}
function changedConnection() { const error = new Error("设备连接已切换，原连接已取消。请核对当前设备与操作记录。"); error.code = "connection_changed"; return error; }
function showConnect() {
  state.sensitiveCleanup?.(); state.sensitiveCleanup = null; state.pair = null; $("pair-status").replaceChildren(); $("pair-status").hidden = true;
  state.authEpoch += 1; state.member = null; state.directory = null; state.messageReadReceipt = null; state.data = null; state.actual.clear(); state.pendingMessages.clear(); state.pending = []; state.drafts.clear(); state.nodes = []; state.conversations = []; state.notifications = []; state.renderKey = ""; state.renderedMessages = []; state.renderedConversation = ""; $("message-text").value = ""; clearInterval(state.timer); state.timer = null; clearTimeout(state.pairTimer);
  state.channel?.close(); state.channel = null;
  document.activeElement?.blur();
  $("tasks").replaceChildren(); $("sessions").replaceChildren(); $("invite-result").replaceChildren(); $("invite-result").hidden = true;
  for (const id of ["conversation-form", "task-form", "session-form", "invite-form"]) $(id).reset();
  render(); $("workspace").hidden = true; $("auth-view").hidden = false; $("disconnect").hidden = true;
  connection(false, "等待连接自己的设备");
}
async function openChannel(entry) {
  showConnect();
  connection(false, "正在核对设备身份");
  state.entry = entry;
  const epoch = state.authEpoch;
  let channel;
  try { channel = await connectDevice(entry, { onEvent: () => scheduleRefresh(), onClose: () => { if (channel && state.channel === channel) { state.channel = null; connection(false, "连接已断开 · 操作不会自动重发"); state.sensitiveCleanup?.(); state.sensitiveCleanup = null; updateComposer(); } } }); }
  catch (error) { if (epoch !== state.authEpoch) throw changedConnection(); throw error; }
  if (epoch !== state.authEpoch) { channel.close(); throw changedConnection(); }
  state.channel = channel;
  state.info = await request("info");
  if (state.info.node_id !== entry.node_id) { channel.close(); throw new Error("入口报告的设备与已核验指纹不一致。"); }
  $("node-label").textContent = nodeName(entry.node_id); $("version-label").textContent = state.info.version || "";
  connection(true, "设备身份已核验 · 端到端加密");
}
async function openEntry(entry) {
  const candidates = [entry, ...savedEntries().filter((item) => item.node_id === entry.node_id && entryKey(item) !== entryKey(entry))];
  let failure;
  for (const candidate of candidates) {
    try { await openChannel(candidate); failure = null; break; }
    catch (error) { if (error.code === "connection_changed") throw error; failure = error; }
  }
  if (failure) throw failure;
  const auth = await request("authenticate", { network_id: entry.network_id || storage.get(`network-for.${entry.node_id}`, "") });
  await enterWorkspace(auth); rememberNetwork(auth);
}
async function enterWorkspace(auth) {
  const member = auth.principal;
  if (!member?.network_id) throw new Error("此浏览器还未获得设备授权。请使用可信邀请完成配对，或导入恢复包。");
  if (state.lastNetwork && state.lastNetwork !== member.network_id) state.conversationId = storage.get(`selected-conversation.${member.network_id}`, "");
  state.lastNetwork = member.network_id;
  state.member = member; state.directory = auth.view;
  state.pendingMessages = new Map(storage.get(networkKey("pending-messages"), []).filter((entry) => Array.isArray(entry) && entry.length === 2 && typeof entry[0] === "string" && typeof entry[1]?.message_id === "string"));
  state.pending = storage.get(networkKey("pending"), []).filter((operation) => operation?.operation_id && operation?.target_node);
  $("auth-view").hidden = true; $("workspace").hidden = false; $("disconnect").hidden = false;
  $("identity-label").textContent = `设备 ${nodeName(clientId)}`;
  $("manage-devices").hidden = !member.can_manage; $("manage-recovery").hidden = !member.can_manage; $("pair-requests-panel").hidden = !member.can_manage;
  notice(""); await refresh(); await request("subscribe", { client_id: clientId });
  state.timer = setInterval(() => { if (state.channel) refresh().catch(showError); }, 15000);
}
let refreshTimer;
function scheduleRefresh() { clearTimeout(refreshTimer); refreshTimer = setTimeout(() => refresh().catch(showError), 350); }
async function refresh() {
  if (!state.member || !state.channel) return;
  if (state.refreshing) { state.refreshAgain = true; return; }
  const authEpoch = state.authEpoch;
  state.refreshing = true; $("refresh").setAttribute("aria-busy", "true");
  try {
    const [nodes, conversations, notifications, directory] = await Promise.all([rpc("nodes"), rpc("conversations"), rpc("notifications", { client_id: clientId }), request("directory")]);
    if (!state.member || authEpoch !== state.authEpoch) return;
    state.directory = directory.view;
    state.nodes = nodes.nodes || []; state.conversations = conversations.conversations || []; state.notifications = notifications.notifications || [];
    if (!state.conversationId && state.conversations.length) state.conversationId = state.conversations[0].conversation_id;
    if (state.conversationId) {
      const conversationId = state.conversationId; const barrier = state.pendingMessages.get(conversationId)?.rejected?.barrier; const readNode = state.entry.node_id;
      const data = await rpc("conversation", { conversation_id: conversationId, basis_heads: frontier() });
      if (authEpoch !== state.authEpoch) return;
      if (conversationId === state.conversationId) { state.data = data; state.messageReadReceipt = { conversation_id: conversationId, node_id: readNode, network_id: state.member.network_id, barrier }; if (!incomplete()) { const frontiers = storage.get(networkKey("frontiers"), {}); frontiers[conversationId] = data.heads || []; storage.set(networkKey("frontiers"), frontiers); } }
    } else state.data = null;
    if (state.member.can_manage) await refreshPairRequests();
    if (authEpoch === state.authEpoch) { render(); notifyBrowser(); }
  } finally { state.refreshing = false; $("refresh").removeAttribute("aria-busy"); if (state.refreshAgain && state.member) { state.refreshAgain = false; scheduleRefresh(); } }
}
function render() {
  renderEntries(); renderConversations(); renderNodes(); renderMessages(); renderTasks(); renderSessions(); renderDevices(); renderNotifications(); renderPending();
  $("conversation-heading").textContent = state.data?.conversation?.title || "开始一个会话";
  $("task-count").textContent = state.data?.tasks?.length || 0; $("notification-count").textContent = state.notifications.length;
  const missing = state.data?.missing;
  $("sync-state").classList.toggle("incomplete", incomplete());
  $("sync-state").textContent = !state.data ? "选择或创建会话后，可发消息和派发任务。" : incomplete() ? `上下文尚未完整，已暂停派发。缺少事件 ${Array.isArray(missing.events) ? missing.events.length : missing.events || 0}，附件 ${Array.isArray(missing.blobs) ? missing.blobs.length : missing.blobs || 0}，冲突 ${Array.isArray(missing.conflicts) ? missing.conflicts.length : missing.conflicts || 0}。` : `此入口已取得已知上下文 · ${(state.data.heads || []).length} 个历史位置。其他离线设备可能仍有未同步记录。`;
  updateComposer();
}
function selectConversation(id) {
  if (state.conversationId !== id) { state.drafts.set(state.conversationId, $("message-text").value); $("message-text").value = state.drafts.get(id) || ""; }
  state.conversationId = id; state.messageReadReceipt = null; storage.set(`selected-conversation.${state.member?.network_id}`, id); state.data = null; state.renderKey = "";
  const url = new URL(location.href); url.searchParams.set("conversation", id); url.searchParams.delete("basis"); url.hash = ""; history.replaceState(null, "", url); render();
}
function renderConversations() {
  $("conversation-list").replaceChildren();
  for (const conversation of state.conversations) { const li = el("li"); const item = button(conversation.title || "未命名会话", async () => { selectConversation(conversation.conversation_id); await refresh(); }); if (conversation.conversation_id === state.conversationId) item.setAttribute("aria-current", "page"); li.append(item); $("conversation-list").append(li); }
  if (!state.conversations.length) $("conversation-list").append(el("li", "muted small", "还没有会话"));
}
function selectOptions(select, nodes, label) {
  const selected = select.value; if (document.activeElement === select) return;
  select.replaceChildren();
  for (const node of nodes) { const option = el("option", "", `${nodeName(node.node_id)}${node.local ? " · 本机" : ""}${node.online ? "" : " · 离线"}`); option.value = node.node_id; select.append(option); }
  if (!nodes.length) { const option = el("option", "", label); option.value = ""; select.append(option); }
  if (nodes.some((node) => node.node_id === selected)) select.value = selected;
}
function renderNodes() {
  selectOptions($("scheduler-node"), state.nodes.filter((node) => node.roles?.includes("scheduler") && node.capabilities?.manager), "没有可用的调度 Agent");
  selectOptions($("executor-node"), state.nodes.filter((node) => node.roles?.includes("executor")), "没有执行设备");
  const node = state.nodes.find((item) => item.node_id === $("executor-node").value); const kinds = node?.capabilities?.executors || [];
  for (const option of $("executor-kind").options) option.disabled = kinds.length > 0 && !kinds.includes(option.value);
  if ($("executor-kind").selectedOptions[0]?.disabled) $("executor-kind").value = kinds[0];
}
function updateComposer() {
  if (!state.member) return;
  const node = state.nodes.find((item) => item.node_id === $("scheduler-node").value);
  const allowed = Boolean(state.channel && state.data && !incomplete() && node?.online && node.capabilities?.manager);
  $("message-form").querySelector('[type="submit"]').disabled = !allowed || $("message-form").querySelector('[type="submit"]').getAttribute("aria-busy") === "true";
  $("manager-hint").textContent = !state.data ? "先创建或选择一个会话。" : incomplete() ? "等待上下文补齐后再发送。" : !node?.capabilities?.manager ? "设备尚未配置调度 Agent。执行会话仍可直接操作。" : !node.online ? "所选调度设备离线，请切换其他入口或设备。" : !state.channel ? "连接已断开，请重新连接并核对状态。" : "消息将交给所选设备上的调度 Agent。";
}
function persistPendingMessages() { storage.set(networkKey("pending-messages"), [...state.pendingMessages], true); }
function renderMessages() {
  const messages = state.data?.messages || []; const key = `${state.conversationId}:${messages.map((item) => item.event_hash || item.message_id).join(",")}`;
  if (key === state.renderKey) return; state.renderKey = key;
  const ids = messages.map((item) => item.event_hash || item.message_id);
  const reset = state.renderedConversation !== state.conversationId || state.renderedMessages.length > ids.length || !state.renderedMessages.every((id, index) => ids[index] === id);
  const offset = reset ? 0 : state.renderedMessages.length;
  if (reset || !offset) $("messages").replaceChildren();
  state.renderedConversation = state.conversationId; state.renderedMessages = ids;
  if (!messages.length) $("messages").append(empty(state.data ? "从你的目标开始" : "准备好继续工作", state.data ? "告诉调度 Agent 要做什么，也可以先创建一张工单。" : "新建会话，或从左侧选择已有会话。"));
  for (const message of messages.slice(offset)) { const article = el("article", `message ${message.role === "user" ? "user" : "assistant"}`); const heading = el("div", "message-role", message.role === "user" ? "你" : message.role === "assistant" ? "调度 Agent" : message.role); if (message.created_at_ms) { const time = el("time", "", new Date(message.created_at_ms).toLocaleString("zh-CN", { month: "numeric", day: "numeric", hour: "2-digit", minute: "2-digit" })); time.dateTime = new Date(message.created_at_ms).toISOString(); heading.append(time); } article.append(heading, el("div", "message-text", message.text)); $("messages").append(article); }
  const pending = state.pendingMessages.get(state.conversationId); const confirmed = pending && messages.find((item) => item.message_id === pending.message_id);
  if (confirmed) { if ($("message-text").value.trim() === confirmed.text) $("message-text").value = ""; state.pendingMessages.delete(state.conversationId); persistPendingMessages(); }
}
function renderTasks() {
  const conversationId = state.conversationId;
  if (editable($("tasks")) && $("tasks").dataset.conversation === conversationId) return; $("tasks").dataset.conversation = conversationId; $("tasks").replaceChildren();
  const tasks = state.data?.tasks || [];
  if (!tasks.length) $("tasks").append(empty("还没有工单", "写下目标和验收条件，再与调度 Agent 讨论。"));
  for (const task of tasks) {
    const card = el("article", "card"); const heading = el("div", "card-heading"); heading.append(el("h3", "", task.title), badge(task.status, task.status === "accepted" ? "已验收" : undefined)); card.append(heading, detailList([["目标", task.goal], ["验收条件", task.acceptance], ["进展", task.detail || "暂无进展"]]));
    const details = el("details"); details.append(el("summary", "", "更新工单状态")); const form = el("form", "stack"); const label = el("label", "", "状态"); const select = el("select"); select.name = "status"; for (const value of ["open", "working", "blocked", "needs_review", "accepted", "cancelled"]) { const option = el("option", "", value === "accepted" ? "已验收" : stateNames[value]); option.value = value; select.append(option); } select.value = task.status; if (!select.value) select.value = "open"; label.append(select); form.append(label); field(form, "说明", "detail", "textarea", task.detail || ""); const submit = el("button", "secondary", "保存状态"); submit.type = "submit"; form.append(submit); form.addEventListener("submit", (event) => { event.preventDefault(); if (form.reportValidity()) perform(submit, async () => { if (conversationId !== state.conversationId) throw new Error("会话已切换，请在当前工单上重新操作。"); await rpc("update_task", { conversation_id: conversationId, task_id: task.task_id, status: select.value, detail: form.elements.detail.value }); document.activeElement.blur(); await refresh(); notice("工单状态已保存。"); }); }); details.append(form); card.append(details); $("tasks").append(card);
  }
}
async function operate(session, action, conversationId = state.conversationId) {
  if (conversationId !== state.conversationId) throw new Error("会话已切换，请在当前执行会话上重新操作。");
  if (!state.data || incomplete()) throw new Error("先补齐当前会话上下文。");
  if (state.pending.some((item) => item.conversation_id === state.conversationId && item.target_node === session.target_node && (action.type === "open" || item.session_id === session.session_id))) throw new Error("此设备有尚未确认的操作，请先核对原操作，再提交新指令。");
  const current = state.data.sessions?.find((item) => item.target_node === session.target_node && item.session_id === session.session_id);
  if (current?.state === "unknown" && action.type !== "read") throw new Error("执行会话结果未知，请先读取实际状态并完成核对。");
  const request = { operation_id: newId(), network_id: state.member.network_id, conversation_id: state.conversationId, task_id: null, target_node: session.target_node, session_id: session.session_id, expected_revision: session.revision, basis_heads: state.data.heads || [], resources: [], action };
  const authEpoch = state.authEpoch;
  const pending = { operation_id: request.operation_id, target_node: request.target_node, session_id: request.session_id, conversation_id: request.conversation_id };
  const nextPending = [...state.pending, pending]; storage.set(networkKey("pending"), nextPending, true); state.pending = nextPending; renderPending();
  try { const result = await rpc("operate", { request }); if (authEpoch !== state.authEpoch) return result; removePending(pending.operation_id); notice(`操作已提交（${stateNames[result.state] || "等待执行结果"}）。`); await refresh(); return result; }
  catch (error) { if (authEpoch === state.authEpoch && error.status >= 400 && error.status < 500) removePending(pending.operation_id); renderPending(); throw error; }
}
function removePending(id) { state.pending = state.pending.filter((item) => item.operation_id !== id); storage.set(networkKey("pending"), state.pending); renderPending(); }
function canReleasePendingMessage(pending) {
  const receipt = state.messageReadReceipt;
  const missing = state.data?.missing;
  return Boolean(pending?.rejected?.barrier && state.channel && receipt?.barrier === pending.rejected.barrier && receipt.node_id === pending.rejected.node_id && receipt.node_id === state.entry?.node_id && receipt.network_id === state.member?.network_id && receipt.conversation_id === pending.conversation_id && pending.conversation_id === state.conversationId && state.data?.conversation?.conversation_id === pending.conversation_id && Array.isArray(state.data.messages) && missing && ["events", "blobs", "conflicts"].every((key) => Object.hasOwn(missing, key)) && !incomplete() && !state.data.messages.some((message) => message.message_id === pending.message_id));
}
function renderPending() {
  $("message-reconciliation").replaceChildren();
  const pendingMessage = state.pendingMessages.get(state.conversationId);
  if (pendingMessage && !state.data?.messages?.some((message) => message.message_id === pendingMessage.message_id)) {
    const card = el("article", "card"); card.append(el("h3", "", "这条消息尚未确认"), el("p", "unknown-note", "保留原消息编号，不会自动重发。请先刷新并核对原入口的聊天记录。"));
    if (canReleasePendingMessage(pendingMessage)) card.append(el("p", "muted small", "原入口已返回错误，随后取得的完整会话未找到此消息。解除只会清除本地待核对记录。"), button("已核对消息未提交，解除本地待核对", () => {
      const current = state.pendingMessages.get(state.conversationId);
      if (current?.message_id !== pendingMessage.message_id || current.rejected?.barrier !== pendingMessage.rejected.barrier || !canReleasePendingMessage(current)) throw new Error("消息或入口状态已变化，请重新刷新核对。");
      const next = [...state.pendingMessages].filter(([id]) => id !== state.conversationId); storage.set(networkKey("pending-messages"), next, true); state.pendingMessages = new Map(next); renderPending(); notice("已解除本地待核对记录，未重发消息。你可以修改内容后重新提交。");
    }, "quiet"));
    $("message-reconciliation").append(card);
  }
  $("pending-operations").replaceChildren();
  for (const operation of state.pending.filter((item) => item.conversation_id === state.conversationId)) { const card = el("article", "card"); card.append(el("h3", "", "有一笔操作尚未确认"), el("p", "unknown-note", "不会自动重发。请向执行设备核对原操作。"), el("code", "", operation.operation_id), button("核对原操作", async () => { const record = await rpc("reconcile", { target_node: operation.target_node, operation_id: operation.operation_id }); card.append(rawDetails("核对结果", record)); const stateValue = record.state || record.operation?.state; if (["succeeded", "failed", "rejected"].includes(stateValue)) { removePending(operation.operation_id); await refresh(); } else if (stateValue === "not_found" && record.target_node === operation.target_node && record.operation === null) { card.append(el("p", "unknown-note", "执行设备当前未找到此编号。确认原请求已结束后，可解除本地待核对记录；不会重发操作。"), button("确认旧请求已结束并解除本地待核对", async () => { removePending(operation.operation_id); notice("已解除本地待核对记录，未重发操作。"); })); } else notice(`原操作状态：${stateNames[stateValue] || "暂未找到，请保持待核对"}。`); })); $("pending-operations").append(card); }
}
function renderSessions() {
  const conversationId = state.conversationId;
  if (editable($("sessions")) && $("sessions").dataset.conversation === conversationId) return; $("sessions").dataset.conversation = conversationId; $("sessions").replaceChildren(); $("operations").replaceChildren();
  const sessions = state.data?.sessions || [];
  if (!sessions.length) $("sessions").append(empty("还没有执行会话", "让调度 Agent 派发任务，或在下方打开指定设备的执行器。"));
  for (const session of sessions) {
    const card = el("article", "card"); const heading = el("div", "card-heading"); heading.append(el("h3", "", `${session.executor} · ${nodeName(session.target_node)}`), badge(session.state)); card.append(heading, detailList([["工作目录", session.cwd], ["会话", session.session_id], ["版本", String(session.revision)]]));
    if (session.state === "unknown") card.append(el("p", "unknown-note", "结果未知。执行器可能仍在运行，先读取实际状态，不要重新派发同一任务。"));
    const actions = el("div", "card-actions"); const read = button("读取实际状态", async () => { const result = await rpc("session", { target_node: session.target_node, session_id: session.session_id }); state.actual.set(`${session.target_node}:${session.session_id}`, result.actual); renderSessions(); notice("已从执行设备读取状态。"); }); read.disabled = !session.native_session_id; actions.append(read); card.append(actions);
    const actual = state.actual.get(`${session.target_node}:${session.session_id}`); if (actual !== undefined) { const details = rawDetails("实际执行状态", actual); details.open = true; card.append(details); renderApprovals(card, session, actual, conversationId); }
    const details = el("details"); details.append(el("summary", "", "给执行器发指令")); const form = el("form", "stack"); const actionLabel = el("label", "", "操作"); const select = el("select"); select.name = "action"; for (const [value, name] of [["send", "发送新一轮任务"], ["steer", "补充当前轮指令"], ["interrupt", "中断当前轮"]]) { const option = el("option", "", name); option.value = value; select.append(option); } actionLabel.append(select); form.append(actionLabel); const turn = field(form, "当前轮 ID（补充或中断时填写）", "turn_id"); turn.required = false; const text = field(form, "内容", "text", "textarea"); select.addEventListener("change", () => { turn.required = select.value !== "send"; text.required = select.value !== "interrupt"; text.closest("label").hidden = select.value === "interrupt"; }); const submit = el("button", "secondary", "提交操作"); submit.type = "submit"; form.append(submit); form.addEventListener("submit", (event) => { event.preventDefault(); if (form.reportValidity()) perform(submit, async () => { const action = { type: select.value }; if (select.value !== "interrupt") action.text = text.value; if (select.value !== "send") action.turn_id = turn.value; await operate(session, action, conversationId); text.value = ""; }); }); details.append(form); card.append(details); $("sessions").append(card);
  }
  const operations = state.data?.operations || []; if (!operations.length) $("operations").append(el("p", "muted small", "暂无操作"));
  for (const operation of operations.slice().reverse()) { const card = el("article", "card"); const heading = el("div", "card-heading"); heading.append(el("code", "", operation.request?.operation_id || "操作"), badge(operation.state)); card.append(heading, rawDetails("操作与结果", operation));
    if (operation.state === "unknown") {
      const form = el("form", "stack");
      form.append(el("p", "unknown-note", "只有你确认原进程和外部结果后，才能解除此操作占用的资源。"));
      const evidence = field(form, "核查证据", "evidence", "textarea"); evidence.minLength = 12;
      const label = el("label", "", "经本人核查的结果"); const outcome = el("select"); outcome.name = "outcome";
      for (const [value, text] of [["failed", "未完成，原执行已结束"], ["succeeded", "已完成，结果已经核实"]]) { const option = el("option", "", text); option.value = value; outcome.append(option); }
      label.append(outcome); form.append(label);
      const submit = el("button", "secondary", "记录核查结果并解除占用"); submit.type = "submit"; form.append(submit);
      form.addEventListener("submit", (event) => { event.preventDefault(); if (form.reportValidity()) perform(submit, async () => { await rpc("resolve_unknown", { target_node: operation.request.target_node, operation_id: operation.request.operation_id, outcome: outcome.value, evidence: evidence.value }); await refresh(); }); });
      card.append(form);
    }
    $("operations").append(card); }
}
function renderApprovals(container, session, actual, conversationId) {
  if (!Array.isArray(actual?.pending_requests)) return;
  for (const request of actual.pending_requests) {
    if (request.params?.threadId && request.params.threadId !== session.native_session_id) continue;
    const section = el("section", "pending-request"); section.append(el("h3", "", "执行器正在等待你的决定"), rawDetails("原始审批请求", request)); const form = el("form", "stack"); const response = field(form, "回复内容（JSON）", "response", "textarea"); response.placeholder = '{"decision":"decline"}'; const submit = el("button", "secondary", "发送人工决定"); submit.type = "submit"; form.append(submit); form.addEventListener("submit", (event) => { event.preventDefault(); if (form.reportValidity()) perform(submit, async () => { let value; try { value = JSON.parse(response.value); } catch { throw new Error("审批回复必须是有效 JSON。请按原始请求的格式填写。"); } if (!value || typeof value !== "object" || Array.isArray(value)) throw new Error("审批回复必须是 JSON 对象。"); await operate(session, { type: "respond_request", request_id: request.id, response: value }, conversationId); state.actual.delete(`${session.target_node}:${session.session_id}`); renderSessions(); }); }); section.append(form); container.append(section);
  }
}
function renderDevices() {
  $("devices").replaceChildren();
  const members = state.directory?.members || [];
  if (!members.length) $("devices").append(empty("还没有设备信息", "连接后读取经过验证的私人设备目录。"));
  const revoked = new Set(state.directory?.revoked || []);
  for (const member of members) {
    if (revoked.has(member.subject_id)) continue;
    const node = state.nodes.find((item) => item.node_id === member.subject_id);
    const self = member.subject_id === clientId;
    const card = el("article", "card"); const heading = el("div", "card-heading");
    heading.append(el("h3", "", `${member.name || nodeName(member.subject_id)}${self ? " · 此浏览器" : ""}`), member.kind === "client" ? badge("ready", member.manage ? "管理客户端" : "客户端") : badge(node?.online ? "ready" : "离线"));
    card.append(heading, el("code", "fingerprint", member.subject_id), detailList([["类型", member.kind === "client" ? "客户端" : "运行节点"], ["角色", member.kind === "client" ? "访问任务与会话" : (member.node_roles || []).map((role) => roleNames[role] || role).join("、") || "未配置"], ["执行器", (node?.capabilities?.executors || []).join("、") || "未配置"], ["调度 Agent", node?.capabilities?.manager ? "已配置" : "未配置"], ["允许目录", (node?.capabilities?.roots || []).join("\n") || "未提供"]]));
    if (state.member?.can_manage && member.manage) card.append(button("通过恢复包管理此客户端", async () => { setView("recovery"); $("recovery-import-form").elements.bundle.focus(); }));
    else if (state.member?.can_manage) {
      const details = el("details");
      details.append(el("summary", "", "移除此设备"), el("p", "muted small", `${self ? "这会撤销此浏览器的管理及访问权限。请确认另有管理设备或恢复包。" : "撤销会阻止设备发起新操作。"}离线设备须同步撤销记录后才会生效，已读取的数据不能被远程抹除。`), button("确认撤销设备授权", async () => {
        const proposal = await request("prepare_revoke", { subject_id: member.subject_id });
        if (proposal.event.action?.type !== "revoke" || proposal.event.action.subject_id !== member.subject_id || proposal.event.issuer !== clientId || proposal.event.network_id !== state.member.network_id) throw new Error("撤销模板与所选设备不一致。");
        proposal.event.signature = signClientValue(proposal.signing, "identity-governance", proposal.event);
        await request("revoke", { event: proposal.event });
        if (self) { showConnect(); notice("此浏览器的授权已撤销。需要重新配对或恢复后才能访问。"); }
        else { await refresh(); notice("设备授权已撤销。"); }
      }, "danger")); card.append(details);
    }
    $("devices").append(card);
  }
}
function renderNotifications() {
  $("notifications").replaceChildren();
  if (!state.notifications.length) $("notifications").append(empty("暂时没有未读通知", "需要你处理的进展会在这里保留，直到你确认已读。"));
  for (const notification of state.notifications) { const card = el("article", "card"); card.append(el("h3", "", notification.kind === "approval" ? "等待人工审批" : "任务进展"), el("p", "message-text", notification.text)); const actions = el("div", "card-actions"); actions.append(button("查看相关会话", async () => { selectConversation(notification.conversation_id); setView(["approval", "needs_input"].includes(notification.kind) ? "sessions" : "chat"); await refresh(); }), button("标为已读", async () => { await rpc("ack", { notification_id: notification.notification_id, client_id: clientId }); await refresh(); }, "quiet")); card.append(actions); $("notifications").append(card); }
  $("enable-notifications").textContent = !("Notification" in globalThis) ? "浏览器不支持提醒" : Notification.permission === "granted" ? (storage.get(networkKey("notify"), false) ? "浏览器提醒已开启" : "启用此网络提醒") : Notification.permission === "denied" ? "请在浏览器设置允许提醒" : "开启浏览器提醒";
  $("enable-notifications").disabled = !("Notification" in globalThis) || Notification.permission === "denied" || (Notification.permission === "granted" && storage.get(networkKey("notify"), false));
}
function notifyBrowser() {
  if (!("Notification" in globalThis) || Notification.permission !== "granted" || !storage.get(networkKey("notify"), false)) return;
  const seen = new Set(storage.get(networkKey("seen"), []));
  for (const item of state.notifications) if (!seen.has(item.notification_id)) { seen.add(item.notification_id); try { const notification = new Notification("Vibeface · 任务进展", { body: item.text, tag: item.notification_id }); notification.onclick = () => { window.focus(); selectConversation(item.conversation_id); setView("chat"); refresh().catch(showError); notification.close(); }; } catch { /* The in-page inbox remains available on unsupported browsers. */ } }
  storage.set(networkKey("seen"), [...seen].slice(-1000));
}
function setView(view) { state.view = view; for (const item of document.querySelectorAll("[data-view]")) { const active = item.dataset.view === view; if (active) item.setAttribute("aria-current", "page"); else item.removeAttribute("aria-current"); } for (const item of document.querySelectorAll(".view")) item.hidden = item.id !== `view-${view}`; }

function isBootstrapInvitation(invitation) { return typeof invitation.capability === "string" && !invitation.invite && !invitation.payload; }
function canonicalValue(value) {
  if (Array.isArray(value)) return value.map(canonicalValue);
  if (value && typeof value === "object") return Object.fromEntries(Object.keys(value).sort().map((key) => [key, canonicalValue(value[key])]));
  return value;
}
function matchingValue(left, right) { return JSON.stringify(canonicalValue(left)) === JSON.stringify(canonicalValue(right)); }
function signingBytes(signing, domain, value) {
  if (signing?.domain !== domain || typeof signing.payload !== "string" || signing.payload.length > 65536) throw new Error("设备返回的签名用途无效，已停止授权。");
  let payload; try { payload = JSON.parse(signing.payload); } catch { throw new Error("设备返回的签名内容无效。"); }
  if (!matchingValue(payload, value)) throw new Error("签名内容与正在确认的设备或权限不一致，已停止授权。");
  return new TextEncoder().encode(signing.payload);
}
async function hashValue(value) {
  const bytes = new TextEncoder().encode(JSON.stringify(canonicalValue(value)));
  return [...new Uint8Array(await crypto.subtle.digest("SHA-256", bytes))].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}
function signClientValue(signing, domain, value) {
  signingBytes(signing, domain, value);
  return signClient(domain, signing.payload);
}
function invitationEntry(invitation) {
  const wrapped = invitation.invite?.invite || invitation.invite || invitation;
  return { address: validEndpoint(invitation.url || wrapped.inviter_url), node_id: validTarget(invitation.node_id || wrapped.inviter_node_id), relay: Boolean(invitation.relay), network_id: wrapped.payload?.network_id || wrapped.network_id };
}
function showBootstrapFields() {
  let bootstrap = false;
  try { bootstrap = isBootstrapInvitation(readInvitation($("pair-invitation").value)); } catch { /* The submit handler reports malformed invitations. */ }
  $("bootstrap-backup").hidden = !bootstrap;
  for (const name of ["passphrase", "confirmation"]) $("pair-form").elements[name].required = bootstrap;
}
function checkPassphrase(form) {
  const password = form.elements.passphrase.value;
  if ([...password].length < 12 || new TextEncoder().encode(password).length > 1024) throw new Error("恢复包口令需要至少 12 个字符，且不超过 1024 字节。");
  if (form.elements.confirmation && password !== form.elements.confirmation.value) throw new Error("两次口令不一致，请重新核对。");
  return password;
}
function sealRecoveryPackage(payload, password) {
  const cleartext = new TextEncoder().encode(JSON.stringify(payload));
  let envelope;
  try { envelope = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(recoverySeal(cleartext, password))); }
  finally { cleartext.fill(0); }
  return new TextEncoder().encode(JSON.stringify({ version: 1, network_id: payload.network_id, recovery_epoch: payload.recovery_epoch, envelope }));
}
async function openRecoveryFile(file, password) {
  if (!file || file.size > 256 * 1024) throw new Error("选择不超过 256 KiB 的加密恢复包。");
  let packageValue;
  try { packageValue = JSON.parse(await file.text()); } catch { throw new Error("恢复包不是有效 JSON。请选择原始加密恢复文件。"); }
  if (packageValue.version !== 1 || !packageValue.envelope || !Number.isSafeInteger(packageValue.recovery_epoch) || packageValue.recovery_epoch < 1) throw new Error("恢复包格式无效或版本不受支持。");
  validTarget(packageValue.network_id);
  let cleartext;
  try { cleartext = recoveryOpen(new TextEncoder().encode(JSON.stringify(packageValue.envelope)), password); }
  catch { throw new Error("无法解锁恢复包。请核对口令，并使用未被修改的原始文件。"); }
  let payload;
  try { payload = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(cleartext)); }
  finally { cleartext.fill(0); }
  let root;
  try {
    root = Identity.fromSecretHex(payload.root_secret_key);
    if (payload.version !== 1 || payload.network_id !== packageValue.network_id || payload.recovery_epoch !== packageValue.recovery_epoch || root.nodeId() !== payload.network_id || payload.genesis?.network_id !== payload.network_id || payload.genesis?.owner_public_key !== root.publicKeyHex() || !Array.isArray(payload.decryption_keys)) throw new Error("恢复包的身份与加密内容不一致。");
    return { payload, root };
  } catch (error) { root?.free(); if (payload) payload.root_secret_key = ""; throw error; }
}
async function restoreRecovery(form) {
  if (!state.client) { state.client = await initClient(); clientId = state.client.nodeId; }
  const name = form.elements.name.value.trim();
  if (!name) throw new Error("填写恢复后此浏览器的设备名称。");
  const password = form.elements.passphrase.value;
  const opened = await openRecoveryFile(form.elements.bundle.files[0], password);
  let released = false;
  const release = () => { if (!released) { released = true; opened.root.free(); opened.payload.root_secret_key = ""; } };
  try {
    const entry = form.elements.address ? readEntry(form) : state.entry;
    if (!entry) throw new Error("先指定并核对自己的目标设备。");
    let capability;
    const bootstrapText = form.elements.bootstrap_invitation?.value.trim();
    if (bootstrapText) {
      const bootstrap = readInvitation(bootstrapText);
      if (!isBootstrapInvitation(bootstrap) || bootstrap.node_id !== entry.node_id || validEndpoint(bootstrap.url) !== entry.address) throw new Error("初始化邀请与恢复目标不一致。请使用此节点本机终端生成的邀请。");
      capability = bootstrap.capability;
    }
    await openChannel(entry);
    if (!state.info.initialized && !capability) throw new Error("新节点恢复需要本机初始化邀请。请在此节点终端取得邀请，核对设备指纹后重试。");
    const challenge = await request("recovery_challenge", { network_id: opened.payload.network_id, owner_public_key: opened.root.publicKeyHex(), genesis: opened.payload.genesis });
    if (challenge.challenge?.network_id !== opened.payload.network_id || challenge.challenge.owner_public_key !== opened.root.publicKeyHex() || challenge.challenge.subject_id !== clientId || challenge.challenge.target_node_id !== entry.node_id || challenge.challenge.challenge_id !== challenge.challenge_id || challenge.challenge.genesis_hash !== await hashValue(opened.payload.genesis) || challenge.challenge.expires_at_ms <= Date.now()) throw new Error("恢复身份验证与当前设备连接不一致。");
    const rootProof = opened.root.sign("identity-recovery-access", signingBytes(challenge.signing, "identity-recovery-access", challenge.challenge));
    const recoveredState = { genesis: opened.payload.genesis, checkpoint: null, events: [] };
    const context = await request("recovery_context", { challenge_id: challenge.challenge_id, signature: rootProof, recovery_epoch: opened.payload.recovery_epoch, ...(!state.info.initialized ? { state: recoveredState, capability } : {}) });
    if (context.view.network_id !== opened.payload.network_id || context.state.genesis.owner_public_key !== opened.root.publicKeyHex()) throw new Error("恢复目标的个人网络与恢复包不一致。");
    const panel = $("pair-status"); panel.hidden = false; panel.replaceChildren(el("h3", "", "确认恢复后保留的设备"), el("p", "recovery-warning", "恢复会建立新的管理授权。请明确选择保留的设备。旧管理客户端默认不保留；已离线设备需要收到恢复记录后才会拒绝旧授权。"), el("code", "fingerprint", `个人网络 ${opened.payload.network_id}`));
    const members = context.view.members.filter((member) => !(context.view.revoked || []).includes(member.subject_id) && member.subject_id !== clientId);
    if (!members.some((member) => member.subject_id === entry.node_id)) members.push({ subject_id: entry.node_id, public_key: state.info.public_key, kind: "node", name: `恢复入口 ${nodeName(entry.node_id)}`, node_roles: [...state.info.roles].sort(), manage: false });
    const recoveryEpoch = state.authEpoch; state.sensitiveCleanup = release;
    const choices = [];
    for (const member of members) {
      const card = el("div", "card"); const label = el("label", "check-label"); const keep = el("input"); keep.type = "checkbox"; keep.checked = !member.manage; keep.disabled = member.subject_id === entry.node_id;
      label.append(keep, document.createTextNode(`保留 ${member.name || nodeName(member.subject_id)}`)); card.append(label, el("code", "fingerprint", member.subject_id));
      let manage;
      if (member.kind === "client") { const adminLabel = el("label", "check-label"); manage = el("input"); manage.type = "checkbox"; adminLabel.append(manage, document.createTextNode("允许此客户端管理设备")); card.append(adminLabel); }
      else card.append(el("p", "muted small", `运行节点 · ${(member.node_roles || []).map((role) => roleNames[role] || role).join("、")}`));
      choices.push({ member, keep, manage }); panel.append(card);
    }
    const newManager = { subject_id: clientId, public_key: state.client.publicKey, kind: "client", name, node_roles: [], manage: true };
    panel.append(el("p", "small", `此浏览器 ${name} 将成为管理客户端。`), el("code", "fingerprint", clientId));
    panel.append(button("签名确认设备并恢复管理权限", async () => {
      if (released || recoveryEpoch !== state.authEpoch) throw new Error("恢复材料已从内存清除。请重新导入恢复包并核对目标设备。");
      try {
        const selected = choices.filter((item) => item.keep.checked).map((item) => ({ ...item.member, manage: item.member.kind === "client" ? item.manage.checked : false })); selected.push(newManager); selected.sort((a, b) => a.subject_id < b.subject_id ? -1 : a.subject_id > b.subject_id ? 1 : 0);
        const prepared = await request("prepare_recovery", { challenge_id: challenge.challenge_id, members: selected });
        const expectedEpoch = Math.max(context.view.epoch, opened.payload.recovery_epoch) + 1;
        if (!Number.isSafeInteger(expectedEpoch) || !matchingValue(prepared.checkpoint.members, selected) || prepared.checkpoint.network_id !== opened.payload.network_id || prepared.checkpoint.epoch !== expectedEpoch) throw new Error("恢复模板与确认保留的设备不一致。");
        prepared.checkpoint.signature = opened.root.sign("identity-recovery", signingBytes(prepared.signing, "identity-recovery", prepared.checkpoint));
        const auth = await request("recover", { challenge_id: challenge.challenge_id, checkpoint: prepared.checkpoint, ...(!state.info.initialized ? { state: context.state, capability } : {}) });
        opened.payload.recovery_epoch = auth.view.epoch;
        const bytes = sealRecoveryPackage(opened.payload, password); downloadRecovery(bytes, opened.payload.network_id); bytes.fill(0);
        rememberNetwork(auth); panel.replaceChildren(); panel.hidden = true; form.reset(); await enterWorkspace(auth); setView("devices"); notice("管理授权已恢复，新的加密恢复包已下载。业务正文和工程文件仍需现存副本。");
      } finally { release(); if (state.sensitiveCleanup === release) state.sensitiveCleanup = null; }
    }, "primary"), button("取消恢复并清除解锁材料", () => { release(); if (state.sensitiveCleanup === release) state.sensitiveCleanup = null; panel.replaceChildren(); panel.hidden = true; notice("已取消恢复，未变更设备授权。"); }, "quiet"));
    form.elements.passphrase.value = ""; if (form.elements.bootstrap_invitation) form.elements.bootstrap_invitation.value = "";
  } catch (error) { release(); throw error; }
}
async function reencryptRecovery(form) {
  const password = checkPassphrase(form);
  const opened = await openRecoveryFile(form.elements.bundle.files[0], form.elements.old_passphrase.value);
  try {
    if (opened.payload.network_id !== state.member?.network_id) throw new Error("恢复包属于其他个人网络。请核对当前连接。");
    const bytes = sealRecoveryPackage(opened.payload, password);
    downloadRecovery(bytes, opened.payload.network_id); bytes.fill(0); form.reset(); notice("新的加密恢复包已下载。原文件仍使用原口令，请分别管理。");
  } finally { opened.root.free(); opened.payload.root_secret_key = ""; form.elements.passphrase.value = ""; form.elements.confirmation.value = ""; form.elements.old_passphrase.value = ""; }
}
function downloadRecovery(bytes, networkId) {
  const url = URL.createObjectURL(new Blob([bytes], { type: "application/json" }));
  const link = el("a"); link.href = url; link.download = `vibeface-recovery-${networkId.slice(0, 12)}.json`; document.body.append(link); link.click(); link.remove(); setTimeout(() => URL.revokeObjectURL(url), 1000);
}
function rememberNetwork(auth) {
  const entry = { ...state.entry, network_id: auth.principal.network_id };
  saveEntry(entry); storage.set(`network-for.${entry.node_id}`, entry.network_id);
}
async function submitPair(form) {
  const invitation = readInvitation(form.elements.invitation.value);
  if (!isBootstrapInvitation(invitation) && (!invitation.invite?.invite || typeof invitation.invite.capability !== "string")) throw new Error("这份内容没有一次性配对授权。请从已授权管理设备创建邀请，或使用恢复包。");
  if (invitation.invite?.invite?.payload?.new_kind === "node") throw new Error("这是运行节点的邀请。请在要加入的电脑或服务器上运行 vibeface-node pair，并使用此邀请。");
  const entry = invitationEntry(invitation);
  const label = form.elements.label.value.trim();
  if (!label) throw new Error("填写此浏览器的设备名称。");
  const password = isBootstrapInvitation(invitation) ? checkPassphrase(form) : null;
  await openChannel(entry);
  form.elements.invitation.value = "";
  $("pair-status").hidden = false; $("pair-status").replaceChildren();
  if (isBootstrapInvitation(invitation)) {
    if (state.info.initialized) throw new Error("此节点已创建个人网络。请从已授权管理设备取得新邀请，或使用恢复包。");
    const root = Identity.generate();
    let prepared; let recoveryBytes;
    try {
      prepared = await request("prepare_genesis", { owner_public_key: root.publicKeyHex(), client_name: label });
      if (prepared.state?.genesis?.network_id !== root.nodeId()) throw new Error("入口返回的个人网络指纹不一致。");
      const initialMembers = prepared.state.genesis.initial_members;
      const ownMember = initialMembers?.find((member) => member.subject_id === clientId);
      const nodeMember = initialMembers?.find((member) => member.subject_id === entry.node_id);
      if (initialMembers?.length !== 2 || !ownMember?.manage || ownMember.kind !== "client" || ownMember.public_key !== state.client.publicKey || nodeMember?.kind !== "node" || nodeMember.manage || nodeMember.public_key !== state.info.public_key || !matchingValue(nodeMember.node_roles, [...state.info.roles].sort()) || prepared.state.genesis.owner_public_key !== root.publicKeyHex()) throw new Error("首台设备的授权模板与当前设备不一致。");
      prepared.state.genesis.signature = root.sign("identity-genesis", signingBytes(prepared.signing, "identity-genesis", prepared.state.genesis));
      const payload = { version: 1, network_id: root.nodeId(), root_secret_key: root.secretHex(), genesis: prepared.state.genesis, recovery_epoch: 1, decryption_keys: [] };
      try { recoveryBytes = sealRecoveryPackage(payload, password); } finally { payload.root_secret_key = ""; }
    } finally { root.free(); form.elements.passphrase.value = ""; form.elements.confirmation.value = ""; }
    const networkId = prepared.state.genesis.network_id;
    let downloaded = false;
    const targetEpoch = state.authEpoch;
    state.sensitiveCleanup = () => { recoveryBytes.fill(0); invitation.capability = ""; };
    const panel = $("pair-status"); panel.append(el("h3", "", "先保存加密恢复包"), el("p", "recovery-warning", "个人私钥已在此浏览器内加密，不会上传节点。请下载恢复包并独立保存口令，再确认创建网络。"), el("code", "fingerprint", `个人网络 ${networkId}`));
    panel.append(button("下载加密恢复包", () => { downloadRecovery(recoveryBytes, networkId); downloaded = true; }));
    const labelElement = el("label", "check-label"); const saved = el("input"); saved.type = "checkbox"; labelElement.append(saved, document.createTextNode("我已保存恢复包，并妥善保存口令")); panel.append(labelElement);
    panel.append(button("确认保存并创建网络", async () => {
      if (targetEpoch !== state.authEpoch) throw changedConnection();
      if (!saved.checked || !downloaded) throw new Error("请先下载恢复包并确认保存。个人私钥无法通过网站密码找回。");
      const auth = await request("bootstrap", { capability: invitation.capability, state: prepared.state });
      state.sensitiveCleanup = null; invitation.capability = ""; form.elements.invitation.value = ""; recoveryBytes.fill(0); panel.replaceChildren(); panel.hidden = true; rememberNetwork(auth); await enterWorkspace(auth); notice("个人网络已创建。后续连接自动使用此浏览器的独立设备密钥。");
    }, "primary"));
    return;
  }
  const prepared = await request("prepare_pair_request", { invite: invitation.invite.invite, capability: invitation.invite.capability, name: label });
  if (prepared.request.member?.subject_id !== clientId || prepared.request.member.public_key !== state.client.publicKey || prepared.request.member.kind !== "client" || prepared.request.member.manage) throw new Error("配对请求与此浏览器的身份或权限不一致。");
  prepared.request.proof = signClientValue(prepared.signing, "identity-pair", [prepared.request.request_id, prepared.request.invite_id, prepared.request.capability, prepared.request.member, prepared.request.nonce]);
  const receipt = await request("pair_request", { request: prepared.request });
  const invitePayload = invitation.invite.invite.payload;
  const expectedProposal = [prepared.request.request_id, prepared.request.invite_id, prepared.request.capability, prepared.request.member, prepared.request.nonce];
  if (receipt.review.version !== 1 || receipt.review.network_id !== invitePayload.network_id || receipt.review.invite_id !== invitePayload.invite_id || receipt.review.epoch !== invitePayload.epoch || receipt.review.inviter_subject_id !== invitePayload.inviter_subject_id || receipt.review.proposal_hash !== await hashValue(expectedProposal) || !matchingValue(receipt.review.new_member, prepared.request.member) || receipt.review.expires_at_ms > invitePayload.expires_at_ms || receipt.review.expires_at_ms <= Date.now()) throw new Error("配对确认与原邀请或已签名的请求不一致，已停止授权。");
  form.elements.invitation.value = "";
  const waiting = { request_id: receipt.request_id, network_id: receipt.review.network_id, entry, review: receipt.review, signing: receipt.signing, accepted: false };
  state.pair = waiting; storage.set("waiting-pair", waiting, true);
  await pollPair();
}
async function pollPair() {
  clearTimeout(state.pairTimer);
  if (!state.pair || !state.channel) return;
  const waiting = state.pair;
  const panel = $("pair-status"); panel.hidden = false; panel.replaceChildren();
  if (!waiting.accepted) {
    const member = waiting.review.new_member;
    if (member.subject_id !== clientId || member.kind !== "client" || member.manage) throw new Error("配对请求返回的设备身份或权限不一致，已停止授权。");
    panel.append(el("h3", "", "确认加入自己的设备网络"), el("p", "muted small", "请核对邀请设备和个人网络。确认后，还需要在管理设备上批准本次请求。"), detailList([["目标设备", waiting.entry.node_id], ["个人网络", waiting.network_id], ["此浏览器", clientId], ["设备名称", member.name], ["权限", "客户端，不包含管理权限"]]), button("确认此浏览器加入", async () => {
      await request("pair_accept", { request_id: waiting.request_id, signature: signClientValue(waiting.signing, "identity-pair-review", waiting.review) });
      waiting.accepted = true; storage.set("waiting-pair", waiting, true); await pollPair();
    }, "primary")); return;
  }
  const result = await request("pair_status", { request_id: waiting.request_id });
  if (state.pair !== waiting) return;
  if (result.status === "granted") {
    const auth = await request("authenticate", { network_id: waiting.network_id });
    state.pair = null; storage.set("waiting-pair", null); panel.replaceChildren(); panel.hidden = true; rememberNetwork(auth); await enterWorkspace(auth); notice("配对完成。此浏览器之后会自动认证。"); return;
  }
  panel.append(el("h3", "", "等待管理设备确认"), el("p", "muted small", "请在发出邀请的管理设备上核对此浏览器指纹，再确认授权。关闭页面不会重新派发业务任务。"), el("code", "fingerprint", clientId), el("p", "small", `请求编号 ${waiting.request_id}`));
  if (result.status !== "pending") throw new Error(`配对未完成（${result.status || "状态未知"}）。请从管理设备核对请求。`);
  state.pairTimer = setTimeout(() => pollPair().catch(showError), 3000);
}
async function refreshPairRequests() {
  const result = await request("pair_requests");
  if (!Array.isArray(result)) throw new Error("设备返回的配对列表格式无效。");
  $("pair-requests").replaceChildren();
  if (!result.length) $("pair-requests").append(el("p", "muted small", "没有等待确认的配对请求。"));
  for (const pending of result) {
    const member = pending.review?.new_member;
    if (!member || !pending.request_id) throw new Error("配对请求缺少可核对的设备身份。");
    if (member.manage) throw new Error("普通邀请不能授予管理权限。请通过恢复包明确授权。");
    validTarget(member.subject_id);
    const card = el("article", "card");
    card.append(el("h3", "", member.name || "待配对设备"), el("p", "muted small", "请与新设备上显示的完整指纹逐字核对。确认后，此设备将获得邀请指定的权限。"), el("code", "fingerprint", member.subject_id), detailList([["类型", member.kind === "node" ? "运行节点" : "客户端"], ["管理权限", member.manage ? "允许管理设备" : "无"], ["节点角色", (member.node_roles || []).map((role) => roleNames[role] || role).join("、") || "无"]]));
    if (!pending.new_subject_signature) card.append(el("p", "muted small", "等待新设备确认加入，随后可在此授权。"));
    else card.append(button("已核对指纹，确认授权", async () => {
      const proposal = await request("prepare_pair", { request_id: pending.request_id });
      if (!matchingValue(proposal.review, pending.review) || !matchingValue(proposal.event.action?.member, member) || proposal.event.action?.type !== "grant" || proposal.event.issuer !== clientId || proposal.event.network_id !== state.member.network_id) throw new Error("配对设备发生变化，请刷新后重新核对。");
      const inviter_signature = signClientValue(proposal.review_signing, "identity-pair-review", proposal.review);
      proposal.event.signature = signClientValue(proposal.signing, "identity-governance", proposal.event);
      await request("pair_confirm", { request_id: pending.request_id, inviter_signature, event: proposal.event }); await refresh(); notice("已签名授权该设备。");
    })); $("pair-requests").append(card);
  }
}
function showInvitation(invitation) {
  const payload = { url: state.entry.address, node_id: state.entry.node_id, relay: Boolean(state.entry.relay), invite: invitation };
  const bytes = new TextEncoder().encode(JSON.stringify(payload));
  const encoded = btoa(String.fromCharCode(...bytes)).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
  const output = $("invite-result"); output.replaceChildren(el("p", "muted small invite-output", "在新设备上打开你信任的 Vibeface 客户端，粘贴以下配对信息。请勿公开分享，新设备申请后还需在此确认。"));
  const label = el("label", "", "一次性配对信息"); const value = el("textarea", "invite-link"); value.readOnly = true; value.value = encoded; label.append(value); output.append(label, button("复制配对信息", async () => { await navigator.clipboard.writeText(value.value); notice("配对信息已复制。请只发送给自己的设备。"); }));
  if (invitation.invite?.payload?.expires_at_ms) output.append(el("p", "muted small", `到期 ${new Date(invitation.invite.payload.expires_at_ms).toLocaleString()}`));
  output.hidden = false;
}
async function start() {
  showConnect();
  state.client = await initClient(); clientId = state.client.nodeId;
  for (const controls of document.querySelectorAll("[data-key-controls]")) controls.disabled = false;
  $("client-fingerprint").textContent = `此浏览器的设备指纹 ${clientId}`; $("recovery-client-id").textContent = clientId;
  if (initialInvitation) { $("pair-invitation").value = JSON.stringify(readInvitation(initialInvitation.value)); initialInvitation.value = ""; initialFragment.delete("bootstrap"); initialFragment.delete("invite"); showBootstrapFields(); return; }
  const waiting = storage.get("waiting-pair", null);
  if (waiting?.request_id && waiting?.entry) { await openChannel(waiting.entry); state.pair = waiting; await pollPair(); return; }
  const entries = savedEntries();
  if (entries.length) await openEntry(entries[entries.length - 1]);
}

$("auth-origin").textContent = location.origin;
bindForm("pair-form", submitPair);
bindForm("recovery-export-form", reencryptRecovery);
bindForm("initial-recovery-form", restoreRecovery);
bindForm("recovery-import-form", restoreRecovery);
$("pair-invitation").addEventListener("input", showBootstrapFields);
$("replace-key").addEventListener("click", () => perform($("replace-key"), async () => { await initClient({ replaceLostKey: true }); $("replace-key").hidden = true; await start(); notice("已生成新的独立设备密钥。请重新配对或恢复。"); }));
$("disconnect").addEventListener("click", () => { showConnect(); notice("已断开。设备密钥和原操作编号仍保存在此浏览器。"); });
bindForm("connect-form", async (form) => openEntry(readEntry(form)));
$("new-conversation").addEventListener("click", () => { $("conversation-form").hidden = false; $("conversation-title").focus(); });
$("cancel-conversation").addEventListener("click", () => { $("conversation-form").hidden = true; $("new-conversation").focus(); });
bindForm("conversation-form", async (form) => { const id = form.dataset.requestId || (form.dataset.requestId = newId()); await rpc("create_conversation", { conversation_id: id, title: form.elements.title.value.trim() }); selectConversation(id); delete form.dataset.requestId; form.reset(); form.hidden = true; setView("chat"); await refresh(); $("message-text").focus(); });
bindForm("entry-form", async (form) => { saveEntry(readEntry(form)); form.reset(); form.closest("details").open = false; notice("已保存固定设备指纹及连接路径。切换路径不会重发任务。"); });
bindForm("message-form", async (form) => { if (!state.data || incomplete()) throw new Error("会话上下文尚未完整。"); const text = form.elements.text.value.trim(); if (!text) throw new Error("先填写消息内容。"); const previous = state.pendingMessages.get(state.conversationId); if (previous?.text && previous.text !== text) throw new Error("此会话有一条消息尚未确认，请先刷新核对原消息，再发送新内容。"); const message = { message_id: previous?.message_id || newId(), conversation_id: state.conversationId, text, scheduler_node: previous?.scheduler_node || $("scheduler-node").value || undefined }; state.pendingMessages.set(state.conversationId, message); persistPendingMessages(); const authEpoch = state.authEpoch; try { await rpc("message", { ...message, basis_heads: state.data.heads || [], scheduler_node: message.scheduler_node }); } catch (error) { if (authEpoch === state.authEpoch && error.responded) { message.rejected = { barrier: newId(), node_id: state.entry.node_id }; state.pendingMessages.set(message.conversation_id, message); persistPendingMessages(); await refresh().catch(() => {}); } renderPending(); throw error; } if (authEpoch !== state.authEpoch) return; if (state.conversationId === message.conversation_id && form.elements.text.value.trim() === message.text) form.elements.text.value = ""; else if (state.drafts.get(message.conversation_id)?.trim() === message.text) state.drafts.delete(message.conversation_id); if (state.pendingMessages.get(message.conversation_id)?.message_id === message.message_id) { state.pendingMessages.delete(message.conversation_id); persistPendingMessages(); } notice("消息已提交，调度 Agent 正在处理。"); await refresh(); });
bindForm("task-form", async (form) => { if (!state.data || incomplete()) throw new Error("先选择上下文完整的会话。"); await rpc("create_task", { conversation_id: state.conversationId, task_id: form.dataset.requestId || (form.dataset.requestId = newId()), title: form.elements.title.value.trim(), goal: form.elements.goal.value.trim(), acceptance: form.elements.acceptance.value.trim(), auto_followup: form.elements.auto_followup.checked }); delete form.dataset.requestId; form.reset(); form.closest("details").open = false; await refresh(); notice("工单已创建。"); });
bindForm("session-form", async (form) => { const target = state.nodes.find((node) => node.node_id === form.elements.target_node.value); if (!target?.online) throw new Error("执行设备离线，请先恢复连接。"); await operate({ target_node: target.node_id, session_id: newId(), revision: 0 }, { type: "open", executor: form.elements.executor.value, cwd: form.elements.cwd.value.trim() }); form.closest("details").open = false; });
bindForm("invite-form", async (form) => {
  if (!state.member?.can_manage) throw new Error("此设备没有管理权限。");
  const kind = form.elements.kind.value;
  const roles = kind === "client" ? [] : [...form.querySelectorAll('[name="role"]:checked')].map((input) => input.value).sort();
  if (kind === "node" && !roles.length) throw new Error("至少选择一个运行节点角色。");
  const prepared = await request("prepare_invite", { kind, node_roles: roles, request_id: newId() });
  if (prepared.invite.payload.new_kind !== kind || !matchingValue(prepared.invite.payload.node_roles, roles) || prepared.invite.payload.manage || prepared.invite.payload.inviter_subject_id !== clientId || prepared.invite.payload.network_id !== state.member.network_id) throw new Error("邀请模板与所选设备权限不一致。");
  prepared.invite.inviter_signature = signClientValue(prepared.signing, "identity-invite", prepared.invite.payload);
  const result = await request("commit_invite", { invite: prepared.invite, capability: prepared.capability });
  showInvitation(result);
});
$("invite-kind").addEventListener("change", () => { $("invite-node-roles").hidden = $("invite-kind").value !== "node"; });
$("scheduler-node").addEventListener("change", updateComposer);
$("executor-node").addEventListener("change", renderNodes);
$("refresh").addEventListener("click", () => perform($("refresh"), async () => { await refresh(); notice("已刷新入口当前可读的状态。"); }));
for (const item of document.querySelectorAll("[data-view]")) item.addEventListener("click", () => setView(item.dataset.view));
$("enable-notifications").addEventListener("click", () => perform($("enable-notifications"), async () => { if (!("Notification" in globalThis)) return; const permission = Notification.permission === "granted" ? "granted" : await Notification.requestPermission(); storage.set(networkKey("notify"), permission === "granted"); if (permission === "granted") { storage.set(networkKey("seen"), state.notifications.map((item) => item.notification_id)); notice("浏览器提醒已开启。已有通知保留在通知列表中。"); } renderNotifications(); }));
window.addEventListener("online", scheduleRefresh);
document.addEventListener("visibilitychange", () => { if (!document.hidden && state.member && state.channel) scheduleRefresh(); });
start().catch((error) => { connection(false, "尚未连接设备"); if (error.code === "key_lost") $("replace-key").hidden = false; showError(error); });
