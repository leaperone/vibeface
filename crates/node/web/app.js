"use strict";

const $ = (id) => document.getElementById(id);
const storage = {
  get(key, fallback) { try { return JSON.parse(localStorage.getItem(`vibeface.${key}`)) ?? fallback; } catch { return fallback; } },
  set(key, value, required = false) { try { localStorage.setItem(`vibeface.${key}`, JSON.stringify(value)); } catch { if (required) throw new Error("浏览器无法保存操作编号。请允许此站点使用本地存储后重试。"); } },
};
const newId = () => globalThis.crypto?.randomUUID?.() ?? `${Date.now().toString(36)}-${Math.random().toString(36).slice(2)}-${Math.random().toString(36).slice(2)}`;
const clientId = storage.get("client", null) || newId();
storage.set("client", clientId);
const state = { account: null, csrf: "", info: {}, register: false, view: "chat", nodes: [], conversations: [], conversationId: new URL(location.href).searchParams.get("conversation") || "", data: null, notifications: [], actual: new Map(), pending: [], pendingMessages: new Map(), drafts: new Map(), stream: null, refreshing: false, refreshAgain: false, timer: null, renderKey: "", renderedMessages: [], renderedConversation: "", authEpoch: 0 };
const roleNames = { gateway: "接入", relay: "中继", replica: "同步", scheduler: "调度", executor: "执行" };
const stateNames = { accepted: "已接收", running: "执行中", unknown: "结果未知", succeeded: "操作完成", failed: "失败", rejected: "已拒绝", open: "已打开", idle: "空闲", pending: "待处理", completed: "已完成", awaiting_acceptance: "待验收", cancelled: "已取消", blocked: "受阻", ready: "就绪" };
const nodeName = (id) => id ? `${id.slice(0, 10)}${id.length > 10 ? "…" : ""}` : "未知设备";
const textOf = (value) => typeof value === "string" ? value : JSON.stringify(value, null, 2);
const el = (tag, className, text) => { const element = document.createElement(tag); if (className) element.className = className; if (text !== undefined) element.textContent = text; return element; };
function showError(error) { $("error").textContent = error instanceof Error ? error.message : String(error); $("error").hidden = false; }
function notice(text) { $("notice").textContent = text; }
function clearError() { $("error").hidden = true; $("error").textContent = ""; }
function connection(online, text) { document.querySelector(".topbar").dataset.connection = online ? "online" : "offline"; $("connection-state").textContent = text; }
function empty(title, detail) { const item = el("div", "empty"); item.append(el("strong", "", title), el("p", "", detail)); return item; }
function badge(value) { return el("span", `badge ${/^[a-z_]+$/.test(value || "") ? value : ""}`, stateNames[value] || value || "未知"); }
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
async function api(path, payload) {
  let response;
  const authEpoch = state.authEpoch;
  try { response = await fetch(path, { method: payload === undefined ? "GET" : "POST", credentials: "same-origin", headers: payload === undefined ? {} : { "Content-Type": "application/json", "X-CSRF-Token": state.csrf }, body: payload === undefined ? undefined : JSON.stringify(payload) }); }
  catch { throw new Error("连接中断，尚未确认操作结果。请刷新或核对操作记录，不要重复派发。"); }
  const data = await response.json().catch(() => ({}));
  if (authEpoch !== state.authEpoch) throw new Error("账户已切换，已忽略上一个账户的请求结果。");
  if (!response.ok) {
    if (response.status === 401 && state.account && authEpoch === state.authEpoch) showAuth();
    const error = new Error(typeof data.error === "string" ? data.error : data.error?.message || data.message || `请求失败（${response.status}）`); error.status = response.status; throw error;
  }
  return data;
}
const rpc = (method, params = {}) => api("/api/rpc", { method, ...params });
function networkKey(key) { return `${key}.${state.account?.network_id || "anonymous"}`; }
function frontier() { return storage.get(networkKey("frontiers"), {})[state.conversationId] || []; }
function incomplete() { const missing = state.data?.missing; return Boolean(missing && ["events", "blobs", "conflicts"].some((key) => Array.isArray(missing[key]) ? missing[key].length : missing[key])); }
function validEntry(value) { const url = new URL(value); if (!["http:", "https:"].includes(url.protocol) || url.username || url.password || url.search || url.hash || (url.pathname !== "/" && url.pathname !== "")) throw new Error("填写 HTTP 或 HTTPS 的入口地址，不包含账号、路径或密钥。"); return url.origin; }
function renderEntries() {
  const entries = [...new Set([location.origin, ...storage.get("entries", []).filter((value) => { try { return validEntry(value) === value; } catch { return false; } })])];
  $("entry-list").replaceChildren();
  for (const origin of entries) {
    const li = el("li"); const anchor = el("a", "", origin.replace(/^https?:\/\//, "")); const url = new URL(origin);
    if (state.conversationId) { url.searchParams.set("conversation", state.conversationId); const basis = JSON.stringify(frontier()); if (basis.length < 6000) url.searchParams.set("basis", basis); }
    anchor.href = url.href; if (origin === location.origin) { anchor.setAttribute("aria-current", "location"); anchor.title = "当前入口"; }
    li.append(anchor);
    if (origin !== location.origin) li.append(button("移除", () => { storage.set("entries", entries.filter((entry) => entry !== origin)); renderEntries(); }, "quiet"));
    $("entry-list").append(li);
  }
}
function showAuth() {
  state.authEpoch += 1; state.account = null; state.csrf = ""; state.data = null; state.actual.clear(); state.pendingMessages.clear(); state.pending = []; state.drafts.clear(); state.nodes = []; state.conversations = []; state.notifications = []; state.renderKey = ""; state.renderedMessages = []; state.renderedConversation = ""; $("message-text").value = ""; state.stream?.close(); state.stream = null; clearInterval(state.timer); state.timer = null;
  document.activeElement?.blur();
  $("tasks").replaceChildren(); $("sessions").replaceChildren();
  $("invite-result").replaceChildren(); $("invite-result").hidden = true;
  for (const id of ["conversation-form", "task-form", "session-form", "invite-form"]) $(id).reset();
  render();
  $("workspace").hidden = true; $("auth-view").hidden = false; $("logout").hidden = true; $("password").value = "";
  connection(true, "入口可达 · 等待登录");
}
async function enterWorkspace(auth) {
  state.authEpoch += 1; state.account = auth.account; state.csrf = auth.csrf;
  if (!state.account?.network_id || !state.csrf) throw new Error("入口没有返回完整的账户会话，请重新登录。");
  const basisText = new URL(location.href).searchParams.get("basis");
  if (basisText && basisText.length < 6000) { try { const heads = JSON.parse(basisText); if (Array.isArray(heads) && heads.every((head) => typeof head === "string")) { const frontiers = storage.get(networkKey("frontiers"), {}); frontiers[state.conversationId] = [...new Set([...(frontiers[state.conversationId] || []), ...heads])]; storage.set(networkKey("frontiers"), frontiers); } } catch { /* Invalid non-secret navigation state is ignored. */ } }
  state.pendingMessages = new Map(storage.get(networkKey("pending-messages"), []).filter((entry) => Array.isArray(entry) && entry.length === 2 && typeof entry[0] === "string" && typeof entry[1]?.message_id === "string"));
  state.pending = storage.get(networkKey("pending"), []).filter((operation) => operation?.operation_id && operation?.target_node);
  $("auth-view").hidden = true; $("workspace").hidden = false; $("logout").hidden = false;
  $("account-label").textContent = state.account.username || "我的账户";
  const authEpoch = state.authEpoch; notice(""); await refresh(); if (authEpoch !== state.authEpoch || !state.account) return; connectEvents(); state.timer = setInterval(() => refresh().catch(showError), 15000);
}
function connectEvents() {
  state.stream?.close(); state.stream = new EventSource(`/api/events?client_id=${encodeURIComponent(clientId)}`);
  state.stream.onopen = () => connection(true, "实时连接已建立");
  state.stream.onerror = () => connection(false, "实时连接中断 · 正在重连");
  state.stream.onmessage = () => scheduleRefresh();
}
let refreshTimer;
function scheduleRefresh() { clearTimeout(refreshTimer); refreshTimer = setTimeout(() => refresh().catch(showError), 350); }
async function refresh() {
  if (!state.account) return;
  if (state.refreshing) { state.refreshAgain = true; return; }
  const authEpoch = state.authEpoch;
  state.refreshing = true; $("refresh").setAttribute("aria-busy", "true");
  try {
    const [nodes, conversations, notifications] = await Promise.all([rpc("nodes"), rpc("conversations"), rpc("notifications", { client_id: clientId })]);
    if (!state.account || authEpoch !== state.authEpoch) return;
    state.nodes = nodes.nodes || []; state.conversations = conversations.conversations || []; state.notifications = notifications.notifications || [];
    if (!state.conversationId && state.conversations.length) state.conversationId = state.conversations[0].conversation_id;
    if (state.conversationId) {
      const conversationId = state.conversationId; const data = await rpc("conversation", { conversation_id: conversationId, basis_heads: frontier() });
      if (authEpoch !== state.authEpoch) return;
      if (conversationId === state.conversationId) { state.data = data; if (!incomplete()) { const frontiers = storage.get(networkKey("frontiers"), {}); frontiers[conversationId] = data.heads || []; storage.set(networkKey("frontiers"), frontiers); } }
    } else state.data = null;
    if (authEpoch === state.authEpoch) { render(); notifyBrowser(); }
  } finally { state.refreshing = false; $("refresh").removeAttribute("aria-busy"); if (state.refreshAgain && state.account) { state.refreshAgain = false; scheduleRefresh(); } }
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
  state.conversationId = id; state.data = null; state.renderKey = "";
  const url = new URL(location.href); url.searchParams.set("conversation", id); url.searchParams.delete("basis"); history.replaceState(null, "", url); render();
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
  if (!state.account) return;
  const node = state.nodes.find((item) => item.node_id === $("scheduler-node").value);
  const allowed = Boolean(state.data && !incomplete() && node?.online && node.capabilities?.manager);
  $("message-form").querySelector('[type="submit"]').disabled = !allowed || $("message-form").querySelector('[type="submit"]').getAttribute("aria-busy") === "true";
  $("manager-hint").textContent = !state.data ? "先创建或选择一个会话。" : incomplete() ? "等待上下文补齐后再发送。" : !node?.capabilities?.manager ? "设备尚未配置调度 Agent。执行会话仍可直接操作。" : !node.online ? "所选调度设备离线，请切换其他入口或设备。" : "消息将交给所选设备上的调度 Agent。";
}
function persistPendingMessages() { storage.set(networkKey("pending-messages"), [...state.pendingMessages].map(([id, message]) => [id, { message_id: message.message_id, conversation_id: id }]), true); }
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
    const card = el("article", "card"); const heading = el("div", "card-heading"); heading.append(el("h3", "", task.title), badge(task.status)); card.append(heading, detailList([["目标", task.goal], ["验收条件", task.acceptance], ["进展", task.detail || "暂无进展"]]));
    const details = el("details"); details.append(el("summary", "", "更新工单状态")); const form = el("form", "stack"); const label = el("label", "", "状态"); const select = el("select"); select.name = "status"; for (const value of ["pending", "running", "blocked", "awaiting_acceptance", "completed", "cancelled"]) { const option = el("option", "", stateNames[value]); option.value = value; select.append(option); } select.value = task.status; if (!select.value) select.value = "pending"; label.append(select); form.append(label); field(form, "说明", "detail", "textarea", task.detail || ""); const submit = el("button", "secondary", "保存状态"); submit.type = "submit"; form.append(submit); form.addEventListener("submit", (event) => { event.preventDefault(); if (form.reportValidity()) perform(submit, async () => { if (conversationId !== state.conversationId) throw new Error("会话已切换，请在当前工单上重新操作。"); await rpc("update_task", { conversation_id: conversationId, task_id: task.task_id, status: select.value, detail: form.elements.detail.value }); document.activeElement.blur(); await refresh(); notice("工单状态已保存。"); }); }); details.append(form); card.append(details); $("tasks").append(card);
  }
}
async function operate(session, action, conversationId = state.conversationId) {
  if (conversationId !== state.conversationId) throw new Error("会话已切换，请在当前执行会话上重新操作。");
  if (!state.data || incomplete()) throw new Error("先补齐当前会话上下文。");
  if (state.pending.some((item) => item.conversation_id === state.conversationId && item.target_node === session.target_node && (action.type === "open" || item.session_id === session.session_id))) throw new Error("此设备有尚未确认的操作，请先核对原操作，再提交新指令。");
  const current = state.data.sessions?.find((item) => item.target_node === session.target_node && item.session_id === session.session_id);
  if (current?.state === "unknown" && action.type !== "read") throw new Error("执行会话结果未知，请先读取实际状态并完成核对。");
  const request = { operation_id: newId(), network_id: state.account.network_id, conversation_id: state.conversationId, task_id: null, target_node: session.target_node, session_id: session.session_id, expected_revision: session.revision, basis_heads: state.data.heads || [], resources: [], action };
  const authEpoch = state.authEpoch;
  const pending = { operation_id: request.operation_id, target_node: request.target_node, session_id: request.session_id, conversation_id: request.conversation_id };
  const nextPending = [...state.pending, pending]; storage.set(networkKey("pending"), nextPending, true); state.pending = nextPending; renderPending();
  try { const result = await rpc("operate", { request }); if (authEpoch !== state.authEpoch) return result; removePending(pending.operation_id); notice(`操作已提交（${stateNames[result.state] || "等待执行结果"}）。`); await refresh(); return result; }
  catch (error) { if (authEpoch === state.authEpoch && error.status >= 400 && error.status < 500) removePending(pending.operation_id); renderPending(); throw error; }
}
function removePending(id) { state.pending = state.pending.filter((item) => item.operation_id !== id); storage.set(networkKey("pending"), state.pending); renderPending(); }
function renderPending() {
  $("pending-operations").replaceChildren();
  for (const operation of state.pending.filter((item) => item.conversation_id === state.conversationId)) { const card = el("article", "card"); card.append(el("h3", "", "有一笔操作尚未确认"), el("p", "unknown-note", "不会自动重发。请向执行设备核对原操作。"), el("code", "", operation.operation_id), button("核对原操作", async () => { const record = await rpc("operation", { target_node: operation.target_node, operation_id: operation.operation_id }); card.append(rawDetails("核对结果", record)); const stateValue = record.state || record.operation?.state; if (["succeeded", "failed", "rejected"].includes(stateValue)) { removePending(operation.operation_id); await refresh(); } else notice(`原操作状态：${stateNames[stateValue] || "暂未找到，请保持待核对"}。`); })); $("pending-operations").append(card); }
}
function renderSessions() {
  const conversationId = state.conversationId;
  if (editable($("sessions")) && $("sessions").dataset.conversation === conversationId) return; $("sessions").dataset.conversation = conversationId; $("sessions").replaceChildren(); $("operations").replaceChildren();
  const sessions = state.data?.sessions || [];
  if (!sessions.length) $("sessions").append(empty("还没有执行会话", "让调度 Agent 派发任务，或在下方打开指定设备的执行器。"));
  for (const session of sessions) {
    const card = el("article", "card"); const heading = el("div", "card-heading"); heading.append(el("h3", "", `${session.executor} · ${nodeName(session.target_node)}`), badge(session.state)); card.append(heading, detailList([["工作目录", session.cwd], ["会话", session.session_id], ["版本", String(session.revision)]]));
    if (session.state === "unknown") card.append(el("p", "unknown-note", "结果未知。执行器可能仍在运行，先读取实际状态，不要重新派发同一任务。"));
    const actions = el("div", "card-actions"); actions.append(button("读取实际状态", async () => { const result = await rpc("session", { target_node: session.target_node, session_id: session.session_id }); state.actual.set(`${session.target_node}:${session.session_id}`, result.actual); renderSessions(); notice("已从执行设备读取状态。"); })); card.append(actions);
    const actual = state.actual.get(`${session.target_node}:${session.session_id}`); if (actual !== undefined) { const details = rawDetails("实际执行状态", actual); details.open = true; card.append(details); renderApprovals(card, session, actual, conversationId); }
    const details = el("details"); details.append(el("summary", "", "给执行器发指令")); const form = el("form", "stack"); const actionLabel = el("label", "", "操作"); const select = el("select"); select.name = "action"; for (const [value, name] of [["send", "发送新一轮任务"], ["steer", "补充当前轮指令"], ["interrupt", "中断当前轮"]]) { const option = el("option", "", name); option.value = value; select.append(option); } actionLabel.append(select); form.append(actionLabel); const turn = field(form, "当前轮 ID（补充或中断时填写）", "turn_id"); turn.required = false; const text = field(form, "内容", "text", "textarea"); select.addEventListener("change", () => { turn.required = select.value !== "send"; text.required = select.value !== "interrupt"; text.closest("label").hidden = select.value === "interrupt"; }); const submit = el("button", "secondary", "提交操作"); submit.type = "submit"; form.append(submit); form.addEventListener("submit", (event) => { event.preventDefault(); if (form.reportValidity()) perform(submit, async () => { const action = { type: select.value }; if (select.value !== "interrupt") action.text = text.value; if (select.value !== "send") action.turn_id = turn.value; await operate(session, action, conversationId); text.value = ""; }); }); details.append(form); card.append(details); $("sessions").append(card);
  }
  const operations = state.data?.operations || []; if (!operations.length) $("operations").append(el("p", "muted small", "暂无操作"));
  for (const operation of operations.slice().reverse()) { const card = el("article", "card"); const heading = el("div", "card-heading"); heading.append(el("code", "", operation.request?.operation_id || "操作"), badge(operation.state)); card.append(heading, rawDetails("操作与结果", operation)); $("operations").append(card); }
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
  if (!state.nodes.length) $("devices").append(empty("还没有设备信息", "刷新以读取此入口已知的个人网络。"));
  for (const node of state.nodes) { const card = el("article", "card"); const heading = el("div", "card-heading"); heading.append(el("h3", "", `${nodeName(node.node_id)}${node.local ? " · 当前设备" : ""}`), badge(node.online ? "ready" : "离线")); card.append(heading, el("code", "", node.node_id), detailList([["角色", (node.roles || []).map((role) => roleNames[role] || role).join("、")], ["执行器", (node.capabilities?.executors || []).join("、") || "未配置"], ["调度 Agent", node.capabilities?.manager ? "已配置" : "未配置"], ["允许目录", (node.capabilities?.roots || []).join("\n") || "未提供"]])); if (!node.local) { const details = el("details"); details.append(el("summary", "", "撤销此设备"), el("p", "muted small", "撤销会阻止设备发起新操作。离线设备要收到撤销信息后才会生效，正在运行的进程不会因此自动停止。"), button("确认撤销设备", async () => { await rpc("revoke", { node_id: node.node_id }); await refresh(); notice("设备授权已撤销。"); }, "danger")); card.append(details); } $("devices").append(card); }
}
function renderNotifications() {
  $("notifications").replaceChildren();
  if (!state.notifications.length) $("notifications").append(empty("暂时没有未读通知", "需要你处理的进展会在这里保留，直到你确认已读。"));
  for (const notification of state.notifications) { const card = el("article", "card"); card.append(el("h3", "", notification.kind === "approval" ? "等待人工审批" : "任务进展"), el("p", "message-text", notification.text)); const actions = el("div", "card-actions"); actions.append(button("查看相关会话", async () => { selectConversation(notification.conversation_id); setView(notification.kind === "approval" ? "sessions" : "chat"); await refresh(); }), button("标为已读", async () => { await rpc("ack", { notification_id: notification.notification_id, client_id: clientId }); await refresh(); }, "quiet")); card.append(actions); $("notifications").append(card); }
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

$("auth-origin").textContent = location.origin;
$("auth-toggle").addEventListener("click", () => { state.register = !state.register; $("auth-title").textContent = state.register ? "创建此入口的账户" : "登录此入口"; $("auth-description").textContent = state.register ? "账户用于访问你的个人设备网络。已配对的设备沿用原网络身份。" : "使用此入口上的账户登录。换到其他地址时需要重新登录。"; $("auth-submit").textContent = state.register ? "创建账户并登录" : "登录"; $("auth-toggle").textContent = state.register ? "已有账户？返回登录" : "第一次使用？创建账户"; $("password").autocomplete = state.register ? "new-password" : "current-password"; clearError(); });
bindForm("auth-form", async (form) => { const credentials = { username: form.elements.username.value.trim(), password: form.elements.password.value }; if (state.register) await api("/api/register", credentials); const auth = await api("/api/login", credentials); $("password").value = ""; await enterWorkspace(auth); });
$("logout").addEventListener("click", () => perform($("logout"), async () => { await api("/api/logout", {}); showAuth(); notice("已退出此入口。"); }));
$("new-conversation").addEventListener("click", () => { $("conversation-form").hidden = false; $("conversation-title").focus(); });
$("cancel-conversation").addEventListener("click", () => { $("conversation-form").hidden = true; $("new-conversation").focus(); });
bindForm("conversation-form", async (form) => { const id = form.dataset.requestId || (form.dataset.requestId = newId()); await rpc("create_conversation", { conversation_id: id, title: form.elements.title.value.trim() }); selectConversation(id); delete form.dataset.requestId; form.reset(); form.hidden = true; setView("chat"); await refresh(); $("message-text").focus(); });
bindForm("entry-form", async (form) => { const origin = validEntry(form.elements.address.value.trim()); storage.set("entries", [...new Set([...storage.get("entries", []), origin])]); form.reset(); form.closest("details").open = false; renderEntries(); notice("入口地址已保存。切换入口后需要在该地址重新登录。"); });
bindForm("message-form", async (form) => { if (!state.data || incomplete()) throw new Error("会话上下文尚未完整。"); const text = form.elements.text.value.trim(); if (!text) throw new Error("先填写消息内容。"); const previous = state.pendingMessages.get(state.conversationId); if (previous?.text && previous.text !== text) throw new Error("此会话有一条消息尚未确认，请先刷新核对原消息，再发送新内容。"); const message = { message_id: previous?.message_id || newId(), conversation_id: state.conversationId, text }; state.pendingMessages.set(state.conversationId, message); persistPendingMessages(); const authEpoch = state.authEpoch; try { await rpc("message", { ...message, basis_heads: state.data.heads || [], scheduler_node: $("scheduler-node").value || undefined }); } catch (error) { if (authEpoch === state.authEpoch && error.status >= 400 && error.status < 500 && error.status !== 409) { state.pendingMessages.delete(message.conversation_id); persistPendingMessages(); } throw error; } if (authEpoch !== state.authEpoch) return; if (state.conversationId === message.conversation_id && form.elements.text.value.trim() === message.text) form.elements.text.value = ""; else if (state.drafts.get(message.conversation_id)?.trim() === message.text) state.drafts.delete(message.conversation_id); if (state.pendingMessages.get(message.conversation_id)?.message_id === message.message_id) { state.pendingMessages.delete(message.conversation_id); persistPendingMessages(); } notice("消息已提交，调度 Agent 正在处理。"); await refresh(); });
bindForm("task-form", async (form) => { if (!state.data || incomplete()) throw new Error("先选择上下文完整的会话。"); await rpc("create_task", { conversation_id: state.conversationId, task_id: form.dataset.requestId || (form.dataset.requestId = newId()), title: form.elements.title.value.trim(), goal: form.elements.goal.value.trim(), acceptance: form.elements.acceptance.value.trim(), auto_followup: form.elements.auto_followup.checked }); delete form.dataset.requestId; form.reset(); form.closest("details").open = false; await refresh(); notice("工单已创建。"); });
bindForm("session-form", async (form) => { const target = state.nodes.find((node) => node.node_id === form.elements.target_node.value); if (!target?.online) throw new Error("执行设备离线，请先恢复连接。"); await operate({ target_node: target.node_id, session_id: newId(), revision: 0 }, { type: "open", executor: form.elements.executor.value, cwd: form.elements.cwd.value.trim() }); form.closest("details").open = false; });
bindForm("invite-form", async (form) => { const roles = [...form.querySelectorAll('[name="role"]:checked')].map((input) => input.value); if (!roles.length) throw new Error("至少选择一个设备角色。"); const result = await rpc("invite", { roles }); $("invite-result").replaceChildren(el("p", "muted small invite-output", "在你要加入的设备上使用此配对信息。配对码可授予访问权限，请只交给你自己的设备。"), el("pre", "", result.command || result.code)); $("invite-result").hidden = false; });
$("scheduler-node").addEventListener("change", updateComposer);
$("executor-node").addEventListener("change", renderNodes);
$("refresh").addEventListener("click", () => perform($("refresh"), async () => { await refresh(); notice("已刷新入口当前可读的状态。"); }));
for (const item of document.querySelectorAll("[data-view]")) item.addEventListener("click", () => setView(item.dataset.view));
$("enable-notifications").addEventListener("click", () => perform($("enable-notifications"), async () => { if (!("Notification" in globalThis)) return; const permission = Notification.permission === "granted" ? "granted" : await Notification.requestPermission(); storage.set(networkKey("notify"), permission === "granted"); if (permission === "granted") { storage.set(networkKey("seen"), state.notifications.map((item) => item.notification_id)); notice("浏览器提醒已开启。已有通知保留在通知列表中。"); } renderNotifications(); }));
window.addEventListener("online", scheduleRefresh);
document.addEventListener("visibilitychange", () => { if (!document.hidden && state.account) scheduleRefresh(); });
(async () => { try { state.info = await api("/api/info"); $("node-label").textContent = nodeName(state.info.node_id); $("version-label").textContent = state.info.version || ""; connection(true, "入口可达"); const auth = await api("/api/me").catch((error) => { if (error.status === 401) return null; throw error; }); if (auth?.account) await enterWorkspace(auth); else showAuth(); } catch (error) { connection(false, "入口连接失败"); showError(error); } })();
