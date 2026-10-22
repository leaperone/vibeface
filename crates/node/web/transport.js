import init, { Handshake, signingMessage, ticketDecode } from "/generated/vibeface_secure.js";

const encoder = new TextEncoder();
const decoder = new TextDecoder("utf-8", { fatal: true });
const devicePattern = /^[a-f0-9]{64}$/;
const prologue = encoder.encode("vibeface:client:v1");
const lostKeyMessage = "此浏览器的设备密钥已被清除。请在管理设备上重新邀请这台浏览器，或导入恢复包。原任务不会因此重新执行。";
let device;
let initializing;

const hex = (bytes) => [...new Uint8Array(bytes)].map((value) => value.toString(16).padStart(2, "0")).join("");

export function validTarget(value) {
  if (typeof value !== "string" || !devicePattern.test(value)) throw new Error("设备编号无效。请使用邀请链接或本机页面提供的入口。");
  return value;
}

export function validEndpoint(value) {
  const url = new URL(value);
  if (!["http:", "https:"].includes(url.protocol) || url.username || url.password || url.search || url.hash || !["", "/"].includes(url.pathname)) throw new Error("填写 HTTP 或 HTTPS 入口地址，不包含路径、账号或密钥。");
  const local = ["localhost", "127.0.0.1", "[::1]"].includes(url.hostname);
  if (location.protocol === "https:" && url.protocol === "http:" && !local) throw new Error("HTTPS 页面只能连接 HTTPS 入口。");
  return url.origin;
}

/** The ticket in a join QR code or link: `vibeface1…` alone or `https://<entry>/join#vibeface1…`. */
export function joinTicket(value) {
  const input = value.trim();
  if (input.startsWith("vibeface1")) return { ticket: input, origin: null };
  try {
    const url = new URL(input);
    if (url.pathname === "/join" && url.hash.startsWith("#vibeface1")) return { ticket: url.hash.slice(1), origin: url.origin };
  } catch {}
  return null;
}

/** Trust comes from the ticket's device key; the address in a link is only a hint. */
export async function parseJoinLink(value) {
  await init();
  if (value.trim().length > 8192) throw new Error("邀请链接过长。请重新复制管理设备上的链接。");
  const found = joinTicket(value);
  if (!found) throw new Error("这不是 Vibeface 邀请链接。");
  const ticket = JSON.parse(ticketDecode(found.ticket));
  let origin = null;
  try { origin = found.origin && validEndpoint(found.origin); } catch {}
  return { origin, entryId: ticket.entry_id, inviteId: ticket.invite_id, inviteHash: ticket.invite_hash, secret: ticket.secret };
}

function database() {
  return new Promise((resolve, reject) => {
    const request = indexedDB.open("vibeface-device", 1);
    request.onupgradeneeded = () => request.result.createObjectStore("keys");
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(new Error("无法打开浏览器设备存储。请允许此站点使用 IndexedDB 后重试。"));
  });
}

/** Runs one transaction on the key store; `operation` returns the request whose result is wanted. */
async function keys(mode, operation) {
  const db = await database();
  try {
    return await new Promise((resolve, reject) => {
      const transaction = db.transaction("keys", mode);
      const request = operation(transaction.objectStore("keys"));
      let result;
      request.onsuccess = () => { result = request.result; };
      request.onerror = (event) => {
        if (request.error?.name === "ConstraintError") { result = false; event.preventDefault(); event.stopPropagation(); }
      };
      transaction.oncomplete = () => resolve(result);
      transaction.onerror = () => reject(new Error("无法保存浏览器设备密钥。请允许持久存储后重试。"));
      transaction.onabort = transaction.onerror;
    });
  } finally { db.close(); }
}

export async function initClient({ replaceLostKey = false } = {}) {
  if (!globalThis.isSecureContext || !globalThis.crypto?.subtle || !globalThis.indexedDB) throw new Error("此地址不能安全保存设备密钥。请在本机使用 localhost，或使用 HTTPS 入口。");
  if (!initializing) initializing = loadDevice(replaceLostKey).catch((error) => { initializing = null; throw error; });
  return initializing;
}

/** A new device key for a browser removed from its network; the old key stays revoked there and is dropped here. */
export async function replaceClient() {
  await initClient();
  await keys("readwrite", (store) => store.delete("device"));
  device = undefined; initializing = undefined;
  return initClient({ replaceLostKey: true });
}

/** The device key is a non-extractable Ed25519 CryptoKey; page scripts can only ask it to sign. */
async function loadDevice(replaceLostKey) {
  await init();
  if (device) return { deviceId: device.deviceId };
  let record = await keys("readonly", (store) => store.get("device"));
  if (!record) {
    let created = false;
    try { created = localStorage.getItem("vibeface.device-created") === "true"; } catch {}
    if (created && !replaceLostKey) { const error = new Error(lostKeyMessage); error.code = "key_lost"; throw error; }
    let pair;
    try { pair = await crypto.subtle.generateKey({ name: "Ed25519" }, false, ["sign", "verify"]); }
    catch { throw new Error("此浏览器不支持 Ed25519 设备密钥。请更新浏览器后重试。"); }
    const candidate = { privateKey: pair.privateKey, deviceId: hex(await crypto.subtle.exportKey("raw", pair.publicKey)), created_at: Date.now() };
    await keys("readwrite", (store) => store.add(candidate, "device"));
    record = await keys("readonly", (store) => store.get("device"));
    try { localStorage.setItem("vibeface.device-created", "true"); } catch {}
  }
  if (!(record?.privateKey instanceof CryptoKey) || record.privateKey.extractable || !devicePattern.test(record.deviceId)) throw new Error("浏览器设备密钥损坏。请重新加入个人网络。");
  device = record;
  return { deviceId: device.deviceId };
}

async function sign(bytes) {
  if (!device) await initClient();
  return hex(await crypto.subtle.sign({ name: "Ed25519" }, device.privateKey, bytes));
}

/** Signs `vibeface:message:v1:{domain}:{sha256(payload)}`, the format every Node verifies. */
export async function signClient(domain, payload) {
  await init();
  return sign(signingMessage(domain, typeof payload === "string" ? encoder.encode(payload) : payload));
}

function openSocket(url, signal, onClose, timeoutMs) {
  const socket = new WebSocket(url);
  socket.binaryType = "arraybuffer";
  const incoming = [], waiting = [];
  let failure;
  const timer = setTimeout(() => close(new Error("连接超时。")), timeoutMs);
  function close(error = new Error("无法接通指定设备。")) {
    if (failure) return;
    failure = error;
    clearTimeout(timer);
    signal?.removeEventListener("abort", abort);
    for (const item of waiting.splice(0)) item.reject(error);
    socket.close();
    onClose?.(error);
  }
  function abort() { close(new Error("设备连接已取消。")); }
  signal?.addEventListener("abort", abort, { once: true });
  socket.addEventListener("message", (event) => {
    if (typeof event.data === "string") {
      // The Gateway refuses in English for logs; `code` is what the page translates.
      const refusal = new Error("访问地址拒绝了这次连接。");
      try { const value = JSON.parse(event.data); refusal.message = String(value.error || refusal.message); refusal.code = value.code; } catch {}
      close(refusal);
      return;
    }
    if (event.data.byteLength > 65535) { close(new Error("设备消息超过通道大小限制。")); return; }
    const item = waiting.shift();
    if (item) item.resolve(event.data);
    else if (incoming.length < 512) incoming.push(event.data);
    else close(new Error("入口发送的数据过多，已关闭连接。"));
  });
  socket.addEventListener("close", () => close());
  socket.addEventListener("error", () => close(new Error("连不上这台设备。请确认它开着，并且和这里网络互通。")));
  if (signal?.aborted) abort();
  return {
    socket, close, ready() { clearTimeout(timer); },
    receive() {
      if (incoming.length) return Promise.resolve(incoming.shift());
      if (failure) return Promise.reject(failure);
      return new Promise((resolve, reject) => waiting.push({ resolve, reject }));
    },
    opened() {
      if (failure) return Promise.reject(failure);
      if (socket.readyState === WebSocket.OPEN) return Promise.resolve();
      return new Promise((resolve, reject) => {
        socket.addEventListener("open", resolve, { once: true });
        socket.addEventListener("close", () => reject(failure || new Error("连接已断开。")), { once: true });
      });
    },
  };
}

/**
 * Opens a Noise channel to `entry.device_id` through the Gateway at `entry.address`.
 * The Node must prove that device key; any other key closes the connection.
 */
export async function connectDevice(entry, { onEvent, onClose, signal, timeoutMs = 10000 } = {}) {
  if (signal?.aborted) throw new Error("设备连接已取消。");
  await initClient();
  const target = validTarget(entry.device_id);
  const endpoint = validEndpoint(entry.address);
  if (entry.local === true && endpoint !== location.origin) throw new Error("本机会话只能连接当前本机入口。");
  const url = new URL(entry.local === true ? "/api/local/ws" : "/api/client", endpoint);
  url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
  const pending = new Map();
  let channel, closed = false, nextId = 0;
  const disconnected = () => new Error("加密连接已断开，操作结果尚未确认。重新连接后先核对原操作，不要重复提交。");
  const wire = openSocket(url.href, signal, (error) => {
    closed = true;
    const reason = channel ? disconnected() : error;
    for (const item of pending.values()) { clearTimeout(item.timer); item.reject(reason); }
    pending.clear();
    onClose?.(reason);
  }, timeoutMs);
  const { socket, receive } = wire;
  const fail = (error) => wire.close(error);
  let handshake;
  try {
    await wire.opened();
    if (entry.local !== true) socket.send(JSON.stringify({ type: "connect", target }));
    handshake = new Handshake(true, target, prologue);
    handshake.setProof(device.deviceId, await sign(handshake.signingBytes()));
    socket.send(handshake.write(new Uint8Array()));
    const response = await receive();
    if (handshake.read(new Uint8Array(response)).length) throw new Error("设备握手包含未预期的数据。");
    socket.send(handshake.write(new Uint8Array()));
    if (!handshake.isFinished() || handshake.remoteId() !== target) throw new Error("设备身份与目标编号不符，已拒绝连接。");
    channel = handshake.finish();
    wire.ready();
  } catch (error) {
    const reason = !error?.code && /device|identity|Proof|signature/i.test(String(error?.message ?? error)) ? new Error("设备身份与目标编号不符，已拒绝连接。") : error;
    fail(reason);
    throw reason;
  } finally { handshake?.free(); }
  (async () => {
    try {
      while (!closed) {
        const plaintext = channel.openRecord(new Uint8Array(await receive()));
        if (plaintext == null) continue;
        const message = JSON.parse(decoder.decode(plaintext));
        if (!message || typeof message !== "object" || Array.isArray(message)) throw new Error("设备返回的加密消息格式无效。");
        if (message.id !== undefined && typeof message.id !== "string") throw new Error("设备返回的请求编号格式无效。");
        if (message.error !== undefined && message.error !== null && typeof message.error !== "string") throw new Error("设备返回的错误格式无效。");
        if (message.id !== undefined && pending.has(message.id)) {
          const item = pending.get(message.id); pending.delete(message.id); clearTimeout(item.timer);
          if (message.error !== undefined && message.error !== null) {
            const error = new Error(message.error); error.responded = true; item.reject(error);
          }
          else item.resolve(message.result);
        } else onEvent?.(message);
      }
    } catch (error) { fail(error); }
    finally { channel.free(); }
  })();
  return {
    deviceId: target,
    request(method, params = {}) {
      if (closed) return Promise.reject(disconnected());
      const id = String(++nextId);
      return new Promise((resolve, reject) => {
        const timer = setTimeout(() => { pending.delete(id); reject(new Error("设备尚未确认请求结果。请核对原操作，不要重复提交。")); }, 60000);
        pending.set(id, { resolve, reject, timer });
        try { for (const record of channel.sealMessage(encoder.encode(JSON.stringify({ id, method, params })))) socket.send(record); }
        catch (error) { fail(error); }
      });
    },
    close() { fail(new Error("已断开设备连接。")); },
  };
}
