import init, { Identity, Handshake, verify } from "/generated/vibeface_secure.js";

const encoder = new TextEncoder();
const decoder = new TextDecoder("utf-8", { fatal: true });
const peerPattern = /^[a-f0-9]{64}$/;
const lostKeyMessage = "此浏览器的设备密钥已被清除。请使用另一台管理设备重新配对，或导入恢复包。原任务不会因此重新执行。";
let identity;
let initializing;

export function validTarget(value) {
  if (typeof value !== "string" || !peerPattern.test(value)) throw new Error("填写完整的 64 位设备编号。请从本机终端或可信邀请中取得，不要使用中继提供的替代编号。");
  return value;
}

export function validEndpoint(value) {
  const url = new URL(value);
  const web = ["http:", "https:"].includes(url.protocol);
  if (value.length > 2048 || !["http:", "https:", "ws:", "wss:"].includes(url.protocol) || url.username || url.password || url.search || url.hash || web && !["", "/"].includes(url.pathname)) throw new Error("填写 HTTP 或 HTTPS 入口，或完整 WS/WSS 端点，不包含账号、查询参数或密钥。");
  const local = ["localhost", "127.0.0.1", "[::1]"].includes(url.hostname);
  if (location.protocol === "https:" && ["http:", "ws:"].includes(url.protocol) && !local) throw new Error("HTTPS 客户端需要 HTTPS 或 WSS 入口。");
  return web ? url.origin : url.href;
}

const canonical = value => JSON.stringify(value, function (_key, item) {
  return item && !Array.isArray(item) && typeof item === "object"
    ? Object.fromEntries(Object.entries(item).sort(([a], [b]) => a < b ? -1 : a > b ? 1 : 0)) : item;
});

async function clusterRecord(value, clusterId) {
  if (!value || typeof value !== "object" || Array.isArray(value) || encoder.encode(JSON.stringify(value)).length > 8192) throw new Error("集群公告格式无效。");
  const keys = ["peer_id", "public_key", "cluster", "direct_urls", "relay_urls", "issued_at", "expires_at", "signature"];
  if (Object.keys(value).length !== keys.length || keys.some(key => !(key in value))) throw new Error("集群公告字段无效。");
  const peerId = validTarget(value.peer_id), publicKey = validTarget(value.public_key);
  const fingerprint = await crypto.subtle.digest("SHA-256", Uint8Array.from(publicKey.match(/../g), byte => parseInt(byte, 16)));
  if ([...new Uint8Array(fingerprint)].map(byte => byte.toString(16).padStart(2, "0")).join("") !== peerId) throw new Error("公告 Peer ID 与公钥不符。");
  const cluster = value.cluster;
  if (!cluster || Object.keys(cluster).length !== 2 || cluster.cluster_id !== clusterId || !Array.isArray(cluster.services) || !cluster.services.includes("client") || cluster.services.some((service, index) => !["peer", "client"].includes(service) || index && ["peer", "client"].indexOf(cluster.services[index - 1]) >= ["peer", "client"].indexOf(service))) throw new Error("公告不属于目标集群 Gateway。");
  const now = Math.floor(Date.now() / 1000);
  if (!Number.isSafeInteger(value.issued_at) || !Number.isSafeInteger(value.expires_at) || value.issued_at > now + 30 || value.expires_at <= now || value.expires_at <= value.issued_at || value.expires_at > value.issued_at + 180) throw new Error("集群公告已过期或有效期无效。");
  for (const urls of [value.direct_urls, value.relay_urls]) {
    if (!Array.isArray(urls) || urls.length > 8 || urls.some(url => typeof url !== "string" || !["ws:", "wss:"].includes(new URL(validEndpoint(url)).protocol))) throw new Error("公告地址无效。");
  }
  if (typeof value.signature !== "string") throw new Error("公告签名无效。");
  verify(publicKey, "reachability-v2", encoder.encode(canonical({ ...value, signature: "" })), value.signature);
  return { peerId, publicKey, expiresAt: value.expires_at, direct: value.direct_urls };
}

function queryCluster(address, clusterId, signal) {
  if (signal?.aborted) return Promise.reject(new Error("集群查询已取消。"));
  const url = new URL(validEndpoint(address));
  if (["http:", "https:"].includes(url.protocol)) { url.pathname = "/api/network/ws"; url.protocol = url.protocol === "https:" ? "wss:" : "ws:"; }
  return new Promise((resolve, reject) => {
    const socket = new WebSocket(url);
    let done = false, welcomed = false;
    const timer = setTimeout(() => finish(new Error("发现服务未响应。")), 8000);
    const abort = () => finish(new Error("集群查询已取消。"));
    function finish(error, records) {
      if (done) return;
      done = true; clearTimeout(timer); signal?.removeEventListener("abort", abort); socket.close();
      if (error) reject(error); else resolve(records);
    }
    signal?.addEventListener("abort", abort, { once: true });
    if (signal?.aborted) { abort(); return; }
    socket.addEventListener("error", () => finish(new Error("无法查询发现服务。")));
    socket.addEventListener("close", () => finish(new Error("发现服务已断开。")));
    socket.addEventListener("message", event => {
      try {
        if (typeof event.data !== "string" || encoder.encode(event.data).length > 16384) throw new Error("发现响应过大或格式无效。");
        const response = JSON.parse(event.data);
        if (!welcomed) {
          if (response?.type !== "welcome") throw new Error("发现服务握手无效。");
          welcomed = true; socket.send(JSON.stringify({ type: "lookup_cluster", cluster_id: clusterId, service: "client" }));
        } else {
          if (response?.type !== "cluster_records" || !Array.isArray(response.records) || response.records.length > 8 || typeof response.truncated !== "boolean") throw new Error("集群发现响应无效。");
          finish(null, response.records);
        }
      } catch (error) { finish(error); }
    });
  });
}

export async function discoverCluster(target, { signal, trustedPeers = [] } = {}) {
  const clusterId = validTarget(target.clusterId);
  if (!Array.isArray(target.directories) || !target.directories.length || target.directories.length > 8) throw new Error("填写 1 至 8 个已知发现服务。");
  const sources = [...new Set(target.directories.map(validEndpoint))];
  const pages = sources.map(() => []);
  let cursor = 0;
  await Promise.all([0, 1].map(async () => {
    while (cursor < sources.length && !signal?.aborted) {
      const index = cursor++;
      try {
        const records = await queryCluster(sources[index], clusterId, signal);
        for (const value of records) {
          try { pages[index].push(await clusterRecord(value, clusterId)); } catch {}
        }
      } catch {}
    }
  }));
  if (signal?.aborted) throw new Error("集群查询已取消。");
  const peers = new Map();
  for (let row = 0; row < 8; row++) for (let source = 0; source < pages.length; source++) {
    const record = pages[source][row];
    if (!record) continue;
    let candidate = peers.get(record.peerId);
    if (!candidate) {
      if (peers.size === 16) continue;
      candidate = { peerId: record.peerId, publicKey: record.publicKey, expiresAt: record.expiresAt, entries: [] };
      peers.set(record.peerId, candidate);
    }
    candidate.expiresAt = Math.min(candidate.expiresAt, record.expiresAt);
    const paths = [{ address: sources[source], peer_id: record.peerId, relay: true }];
    if (trustedPeers.includes(record.peerId)) paths.push(...record.direct.map(address => ({ address, peer_id: record.peerId, relay: false })));
    for (const entry of paths) if (candidate.entries.length < 2 && !candidate.entries.some(item => item.address === entry.address && item.relay === entry.relay)) candidate.entries.push(entry);
  }
  return [...peers.values()];
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
    catch { throw new Error("无法读取邀请。请粘贴完整邀请链接或终端输出的邀请内容。"); }
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

export async function connectDevice(entry, { onEvent, onClose, signal } = {}) {
  if (signal?.aborted) throw new Error("设备连接已取消。");
  if (!identity) await initClient();
  const target = validTarget(entry.peer_id);
  const endpoint = validEndpoint(entry.address);
  if (entry.local === true && (entry.relay || endpoint !== location.origin)) throw new Error("本机会话只能连接当前本机入口。");
  const url = new URL(endpoint);
  if (["http:", "https:"].includes(url.protocol)) { url.pathname = entry.local === true ? "/api/local/ws" : "/api/network/ws"; url.protocol = url.protocol === "https:" ? "wss:" : "ws:"; }
  const socket = new WebSocket(url);
  socket.binaryType = "arraybuffer";
  const incoming = [];
  const waiting = [];
  const pending = new Map();
  let channel;
  let publicKey;
  let closed = false;
  let nextId = 0;
  const disconnected = () => new Error("加密连接已断开，操作结果尚未确认。重新连接后先核对原操作，不要重复派发。");
  function fail(error) {
    if (closed) return;
    closed = true;
    for (const item of waiting.splice(0)) item.reject(error);
    for (const item of pending.values()) { clearTimeout(item.timer); item.reject(error); }
    pending.clear();
    signal?.removeEventListener("abort", abort);
    socket.close();
    onClose?.(error);
  }
  function abort() { fail(new Error("设备连接已取消。")); }
  signal?.addEventListener("abort", abort, { once: true });
  if (signal?.aborted) abort();
  function receive() {
    if (incoming.length) return Promise.resolve(incoming.shift());
    if (closed) return Promise.reject(disconnected());
    return new Promise((resolve, reject) => waiting.push({ resolve, reject }));
  }
  socket.addEventListener("message", (event) => {
    if ((typeof event.data === "string" && event.data.length > 8192) || (event.data instanceof ArrayBuffer && event.data.byteLength > 65535)) { fail(new Error("设备消息超过通道大小限制。")); return; }
    const item = waiting.shift();
    if (item) item.resolve(event.data);
    else if (incoming.length < 512) incoming.push(event.data);
    else fail(new Error("入口发送的数据过多，已关闭连接。请重新连接可信设备。"));
  });
  socket.addEventListener("close", () => fail(disconnected()));
  socket.addEventListener("error", () => fail(new Error("无法连接设备。请检查地址、HTTPS 和设备在线状态。")));
  const handshakeTimer = setTimeout(() => fail(new Error("连接设备超时。请核对设备编号、地址或中继是否在线。")), 20000);
  async function control(expected) {
    const frame = await receive();
    if (typeof frame !== "string" || frame.length > 8192) throw new Error("中继握手格式无效。");
    const message = JSON.parse(frame);
    if (!message || typeof message !== "object" || Array.isArray(message)) throw new Error("中继握手格式无效。");
    if (message.type !== expected) throw new Error(typeof message.message === "string" ? message.message : "入口未能建立指定设备的通道。");
    return message;
  }
  let handshake;
  try {
    if (entry.local === true) {
      await new Promise((resolve, reject) => {
        if (socket.readyState === WebSocket.OPEN) { resolve(); return; }
        socket.addEventListener("open", resolve, { once: true });
        socket.addEventListener("close", () => reject(disconnected()), { once: true });
        socket.addEventListener("error", () => reject(disconnected()), { once: true });
      });
    } else {
      await control("welcome");
      socket.send(JSON.stringify(entry.relay ? { type: "dial", target, service: "client" } : { type: "direct", service: "client" }));
      await control("ready");
    }
    handshake = new Handshake(true, identity.secretHex(), target, encoder.encode("vibeface:client:v1"));
    socket.send(handshake.write(new Uint8Array()));
    const response = await receive();
    if (!(response instanceof ArrayBuffer)) throw new Error("设备未返回加密握手。");
    if (handshake.read(new Uint8Array(response)).length) throw new Error("设备握手包含未预期的数据。");
    socket.send(handshake.write(new Uint8Array()));
    if (!handshake.isFinished() || handshake.remoteId() !== target) throw new Error("设备身份与已保存设备编号不符，已拒绝连接。");
    publicKey = handshake.remotePublicKey();
    if (typeof publicKey !== "string" || !peerPattern.test(publicKey)) throw new Error("设备未提供有效的认证公钥。");
    channel = handshake.finish();
    clearTimeout(handshakeTimer);
  } catch (error) { clearTimeout(handshakeTimer); fail(error); throw error; }
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
