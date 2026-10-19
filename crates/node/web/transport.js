import init, { Identity, Handshake, verify } from "/generated/vibeface_secure.js";

const encoder = new TextEncoder();
const decoder = new TextDecoder("utf-8", { fatal: true });
const peerPattern = /^[a-f0-9]{64}$/;
const lostKeyMessage = "此浏览器的设备密钥已被清除。请使用另一台管理设备重新配对，或导入恢复包。原任务不会因此重新执行。";
let identity;
let initializing;

export function validTarget(value) {
  if (typeof value !== "string" || !peerPattern.test(value)) throw new Error("填写完整的 64 位设备编号。请从节点本机页面或可信邀请中取得，不要使用中继提供的替代编号。");
  return value;
}

export function validEndpoint(value) {
  const url = new URL(value);
  if (!["http:", "https:"].includes(url.protocol) || url.username || url.password || url.search || url.hash || !["", "/"].includes(url.pathname)) throw new Error("填写 HTTP 或 HTTPS 入口地址，不包含路径、账号或密钥。");
  const local = ["localhost", "127.0.0.1", "[::1]"].includes(url.hostname);
  if (location.protocol === "https:" && url.protocol === "http:" && !local) throw new Error("HTTPS 客户端需要 HTTPS 入口。请为目标或中继启用 HTTPS，再保留相同设备编号连接。");
  return url.origin;
}

export function readInvitation(value) {
  const input = value.trim();
  if (input.length > 65536) throw new Error("邀请内容超过大小限制。请重新复制管理设备生成的邀请。");
  let content = input;
  if (/^https?:\/\//.test(input)) {
    const url = new URL(input);
    const fragment = new URLSearchParams(url.hash.slice(1));
    content = fragment.get("invite") || fragment.get("bootstrap") || "";
  }
  if (!content) throw new Error("邀请链接缺少配对信息。请从管理设备重新复制完整链接。");
  let invitation;
  try { invitation = JSON.parse(content); }
  catch {
    try { invitation = JSON.parse(new TextDecoder().decode(Uint8Array.from(atob(content.replace(/-/g, "+").replace(/_/g, "/")), (character) => character.charCodeAt(0)))); }
    catch { throw new Error("无法读取邀请。请粘贴完整邀请链接或管理客户端复制的邀请内容。"); }
  }
  if (!invitation || typeof invitation !== "object" || Array.isArray(invitation)) throw new Error("邀请内容格式无效。");
  return invitation;
}

function database() {
  return new Promise((resolve, reject) => {
    const request = indexedDB.open("vibeface-device", 1);
    request.onupgradeneeded = () => request.result.createObjectStore("keys");
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(new Error("无法打开浏览器设备存储。请允许此站点使用 IndexedDB 后重试。"));
  });
}

async function keyRecord(write) {
  const db = await database();
  try {
    return await new Promise((resolve, reject) => {
      const transaction = db.transaction("keys", write ? "readwrite" : "readonly");
      const request = write ? transaction.objectStore("keys").put(write, "identity") : transaction.objectStore("keys").get("identity");
      let result;
      request.onsuccess = () => { result = request.result; };
      transaction.oncomplete = () => resolve(result);
      transaction.onerror = () => reject(new Error("无法保存浏览器设备密钥。请允许持久存储后重试。"));
      transaction.onabort = transaction.onerror;
    });
  } finally { db.close(); }
}

async function claimKey(record) {
  const db = await database();
  try {
    return await new Promise((resolve, reject) => {
      const transaction = db.transaction("keys", "readwrite");
      const request = transaction.objectStore("keys").add(record, "identity");
      let alreadyStored = false;
      request.onerror = (event) => {
        if (request.error?.name === "ConstraintError") { alreadyStored = true; event.preventDefault(); event.stopPropagation(); return; }
        reject(new Error("无法保存浏览器设备密钥。请允许持久存储后重试。"));
      };
      transaction.oncomplete = () => resolve(!alreadyStored);
      transaction.onerror = () => reject(new Error("无法保存浏览器设备密钥。请允许持久存储后重试。"));
      transaction.onabort = transaction.onerror;
    });
  } finally { db.close(); }
}

export async function initClient({ replaceLostKey = false } = {}) {
  if (!globalThis.isSecureContext || !globalThis.crypto?.getRandomValues || !globalThis.indexedDB) throw new Error("此地址不能安全保存设备密钥。请在本机使用 http://localhost:4790，或使用你信任的 HTTPS 客户端连接设备。局域网 HTTP 页面仅能展示此指引，不会降级认证。");
  if (!initializing) initializing = initializeIdentity(replaceLostKey).catch((error) => { initializing = null; throw error; });
  return initializing;
}

async function initializeIdentity(replaceLostKey) {
  await init();
  if (!identity) {
    const record = await keyRecord();
    if (record?.secret) identity = Identity.fromSecretHex(record.secret);
    else {
      let previouslyPaired = false;
      try { previouslyPaired = localStorage.getItem("vibeface.device-created") === "true"; } catch {}
      if (previouslyPaired && !replaceLostKey) { const error = new Error(lostKeyMessage); error.code = "key_lost"; throw error; }
      const generated = Identity.generate();
      let claimed;
      try { claimed = await claimKey({ secret: generated.secretHex(), created_at: Date.now() }); }
      catch (error) { generated.free(); throw error; }
      if (claimed) identity = generated;
      else { generated.free(); const existing = await keyRecord(); if (!existing?.secret) throw new Error("浏览器设备密钥初始化发生竞争，请刷新后重试。"); identity = Identity.fromSecretHex(existing.secret); }
      try { localStorage.setItem("vibeface.device-created", "true"); } catch {}
    }
  }
  return { peerId: identity.peerId(), publicKey: identity.publicKeyHex() };
}

export function signClient(domain, payload) {
  if (!identity) throw new Error("浏览器设备密钥尚未初始化。");
  return identity.sign(domain, encoder.encode(payload));
}

function networkUrl(value) {
  const url = new URL(value);
  if (value.length > 2048 || !["ws:", "wss:"].includes(url.protocol) || !url.hostname || url.username || url.password || url.search || url.hash) throw new Error("设备提供的连接地址无效或不安全。");
  return url.href;
}

function openSocket(url, signal, onClose) {
  const socket = new WebSocket(url);
  socket.binaryType = "arraybuffer";
  const incoming = [], waiting = [];
  let failure;
  const timer = setTimeout(() => close(new Error("此路径连接超时。")), 10000);
  function close(error = new Error("此路径无法接通指定设备。")) {
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
    if ((typeof event.data === "string" && encoder.encode(event.data).byteLength > 16384) || (event.data instanceof ArrayBuffer && event.data.byteLength > 65535)) { close(new Error("设备消息超过通道大小限制。")); return; }
    const item = waiting.shift();
    if (item) item.resolve(event.data);
    else if (incoming.length < 512) incoming.push(event.data);
    else close(new Error("入口发送的数据过多，已关闭连接。"));
  });
  socket.addEventListener("close", () => close());
  socket.addEventListener("error", () => close(new Error("无法连接此路径，请检查地址和设备在线状态。")));
  if (signal?.aborted) abort();
  function receive() {
    if (incoming.length) return Promise.resolve(incoming.shift());
    if (failure) return Promise.reject(failure);
    return new Promise((resolve, reject) => waiting.push({ resolve, reject }));
  }
  async function control(expected) {
    const frame = await receive();
    if (typeof frame !== "string") throw new Error("入口连接响应格式无效。");
    const message = JSON.parse(frame);
    if (!message || typeof message !== "object" || Array.isArray(message) || message.type !== expected) throw new Error("入口未能建立指定设备的通道。");
    return message;
  }
  return {
    socket, receive, control, close, ready() { clearTimeout(timer); },
    opened() {
      if (failure) return Promise.reject(failure);
      if (socket.readyState === WebSocket.OPEN) return Promise.resolve();
      return new Promise((resolve, reject) => {
        socket.addEventListener("open", resolve, { once: true });
        socket.addEventListener("close", () => reject(failure || new Error("本机连接已断开。")), { once: true });
        socket.addEventListener("error", () => reject(failure || new Error("无法连接本机入口。")), { once: true });
      });
    },
  };
}

async function lookupPaths(url, target, signal) {
  const wire = openSocket(url, signal);
  try {
    await wire.control("welcome");
    wire.socket.send(JSON.stringify({ type: "lookup", target }));
    const { record } = await wire.control("record");
    if (record === null) return [];
    const fields = ["peer_id", "public_key", "direct_urls", "relay_urls", "issued_at", "expires_at", "signature"];
    const now = Math.floor(Date.now() / 1000);
    if (!record || typeof record !== "object" || Array.isArray(record) || Object.keys(record).length !== fields.length || fields.some(key => !Object.hasOwn(record, key)) || record.peer_id !== target || !peerPattern.test(record.public_key) || !/^[a-f0-9]{128}$/.test(record.signature) || !Number.isSafeInteger(record.issued_at) || !Number.isSafeInteger(record.expires_at) || record.issued_at > now + 30 || record.expires_at <= now || record.expires_at <= record.issued_at || record.expires_at > record.issued_at + 180) throw new Error("入口提供的设备地址记录无效或已过期。");
    for (const urls of [record.direct_urls, record.relay_urls]) {
      if (!Array.isArray(urls) || urls.length > 8 || urls.some(value => typeof value !== "string")) throw new Error("设备地址记录超出限制。");
      for (const value of urls) networkUrl(value);
    }
    const publicBytes = Uint8Array.from(record.public_key.match(/../g), value => parseInt(value, 16));
    const digest = new Uint8Array(await crypto.subtle.digest("SHA-256", publicBytes));
    if ([...digest].map(value => value.toString(16).padStart(2, "0")).join("") !== target) throw new Error("设备地址记录与目标 Peer ID 不符。");
    const unsigned = Object.fromEntries(fields.map(key => [key, key === "signature" ? "" : record[key]]));
    verify(record.public_key, "reachability", encoder.encode(JSON.stringify(unsigned)), record.signature);
    return [
      ...record.direct_urls.map(value => ({ url: networkUrl(value), relay: false, expires: record.expires_at })),
      ...record.relay_urls.map(value => ({ url: networkUrl(value), relay: true, expires: record.expires_at })),
    ].filter(candidate => {
      const url = new URL(candidate.url);
      return location.protocol !== "https:" || url.protocol === "wss:" || ["localhost", "127.0.0.1", "[::1]"].includes(url.hostname);
    });
  } finally { wire.close(); }
}

export async function connectDevice(entry, { onEvent, onClose, signal } = {}) {
  if (signal?.aborted) throw new Error("设备连接已取消。");
  if (!identity) await initClient();
  const target = validTarget(entry.peer_id);
  const endpoint = validEndpoint(entry.address);
  if (entry.local === true) {
    if (endpoint !== location.origin) throw new Error("本机会话只能连接当前本机入口。");
    const local = new URL("/api/local/ws", endpoint);
    local.protocol = local.protocol === "https:" ? "wss:" : "ws:";
    return connectPath(local.href, target, null, { onEvent, onClose, signal });
  }
  const initial = new URL("/api/network/ws", endpoint);
  initial.protocol = initial.protocol === "https:" ? "wss:" : "ws:";
  const entryUrl = initial.href;
  return new Promise((resolve, reject) => {
    const queue = [], active = new Map(), seen = new Set();
    const lookup = new AbortController();
    let winner, finished = false, looking = true, lastError, direct = 0, relay = 0;
    const timer = setTimeout(() => stop(new Error("无法连接指定设备，请检查设备是否在线及入口地址。")), 15000);
    function cleanup() { clearTimeout(timer); signal?.removeEventListener("abort", abort); lookup.abort(); }
    function stop(error) {
      cleanup();
      if (winner) { winner.controller.abort(); return; }
      if (finished) return;
      finished = true;
      for (const attempt of active.values()) attempt.controller.abort();
      reject(error);
    }
    function abort() { stop(new Error("设备连接已取消。")); }
    signal?.addEventListener("abort", abort, { once: true });
    function add(candidate) {
      const key = `${candidate.relay}:${candidate.url}`;
      if (seen.has(key)) return;
      if (candidate.relay ? relay >= 8 : direct >= 16) return;
      seen.add(key);
      if (candidate.relay) relay++; else direct++;
      queue.push(candidate);
    }
    function pump() {
      if (finished) return;
      while (active.size < 2 && queue.length) {
        const needRelay = active.size && ![...active.values()].some(item => item.relay);
        const preferred = queue.findIndex(item => item.relay === Boolean(needRelay));
        const candidate = queue.splice(preferred < 0 ? 0 : preferred, 1)[0];
        if (candidate.expires && candidate.expires <= Math.floor(Date.now() / 1000)) continue;
        const attempt = { ...candidate, controller: new AbortController() };
        active.set(attempt, attempt);
        connectPath(candidate.url, target, candidate.relay, {
          signal: attempt.controller.signal,
          onEvent: event => { if (winner === attempt) onEvent?.(event); },
          onClose: error => { if (winner === attempt) { cleanup(); onClose?.(error); } },
        }).then(channel => {
          active.delete(attempt);
          if (finished) { channel.close(); return; }
          winner = attempt; finished = true; clearTimeout(timer); lookup.abort();
          for (const other of active.values()) other.controller.abort();
          resolve(channel);
        }, error => {
          active.delete(attempt);
          if (finished) return;
          lastError = error;
          pump();
        });
      }
      if (!active.size && !queue.length && !looking) stop(lastError || new Error("没有找到此设备的可达路径。"));
    }
    add({ url: entryUrl, relay: true });
    pump();
    lookupPaths(entryUrl, target, lookup.signal).then(candidates => {
      looking = false;
      if (finished) return;
      for (const candidate of candidates) {
        if (candidate.url !== entryUrl || candidate.relay) add(candidate);
      }
      pump();
    }, error => {
      looking = false;
      lastError ??= error;
      pump();
    });
    if (signal?.aborted) abort();
  });
}

async function connectPath(url, target, relay, { onEvent, onClose, signal }) {
  const pending = new Map();
  let channel, closed = false, nextId = 0, publicKey;
  const disconnected = () => new Error("加密连接已断开，操作结果尚未确认。重新连接后先核对原操作，不要重复派发。");
  const wire = openSocket(url, signal, error => {
    closed = true;
    const reason = channel ? disconnected() : error;
    for (const item of pending.values()) { clearTimeout(item.timer); item.reject(reason); }
    pending.clear();
    onClose?.(reason);
  });
  const { socket, receive } = wire;
  const fail = error => wire.close(error);
  let handshake;
  try {
    if (relay === null) await wire.opened();
    else {
      await wire.control("welcome");
      socket.send(JSON.stringify(relay ? { type: "dial", target, service: "client" } : { type: "direct", service: "client" }));
      await wire.control("ready");
    }
    handshake = new Handshake(true, identity.secretHex(), target, encoder.encode("vibeface:client:v1"));
    socket.send(handshake.write(new Uint8Array()));
    const response = await receive();
    if (!(response instanceof ArrayBuffer)) throw new Error("设备未返回加密握手。");
    if (handshake.read(new Uint8Array(response)).length) throw new Error("设备握手包含未预期的数据。");
    socket.send(handshake.write(new Uint8Array()));
    if (!handshake.isFinished() || handshake.remoteId() !== target) throw new Error("设备身份与目标 Peer ID 不符，已拒绝连接。");
    publicKey = handshake.remotePublicKey();
    if (typeof publicKey !== "string" || !peerPattern.test(publicKey)) throw new Error("设备未提供有效的认证公钥。");
    channel = handshake.finish();
    wire.ready();
  } catch (error) { fail(error); throw error; }
  finally { handshake?.free(); }
  (async () => {
    try {
      while (!closed) {
        const frame = await receive();
        if (!(frame instanceof ArrayBuffer)) throw new Error("加密通道收到未加密的数据，已关闭连接。");
        const plaintext = channel.openRecord(new Uint8Array(frame));
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
    peerId: target,
    publicKey,
    request(method, params = {}) {
      if (closed) return Promise.reject(disconnected());
      const id = String(++nextId);
      return new Promise((resolve, reject) => {
        const timer = setTimeout(() => { pending.delete(id); reject(new Error("设备尚未确认请求结果。请核对原操作，不要重复派发。")); }, 60000);
        pending.set(id, { resolve, reject, timer });
        try { for (const record of channel.sealMessage(encoder.encode(JSON.stringify({ id, method, params })))) socket.send(record); }
        catch (error) { fail(error); }
      });
    },
    close() { fail(new Error("已断开设备连接。")); },
  };
}
