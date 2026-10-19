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

async function verifyReachability(value) {
  if (!value || typeof value !== "object" || Array.isArray(value) || encoder.encode(JSON.stringify(value)).length > 8192) throw new Error("集群公告格式无效。");
  const keys = ["peer_id", "public_key", "cluster", "direct_urls", "relay_urls", "issued_at", "expires_at", "signature"];
  if (Object.keys(value).length !== keys.length || keys.some(key => !(key in value))) throw new Error("集群公告字段无效。");
  const peerId = validTarget(value.peer_id), publicKey = validTarget(value.public_key);
  const fingerprint = await crypto.subtle.digest("SHA-256", Uint8Array.from(publicKey.match(/../g), byte => parseInt(byte, 16)));
  if ([...new Uint8Array(fingerprint)].map(byte => byte.toString(16).padStart(2, "0")).join("") !== peerId) throw new Error("公告 Peer ID 与公钥不符。");
  const cluster = value.cluster;
  if (cluster !== null) {
    if (!cluster || typeof cluster !== "object" || Array.isArray(cluster) || Object.keys(cluster).length !== 2 || !Object.hasOwn(cluster, "cluster_id") || !Object.hasOwn(cluster, "services")) throw new Error("公告集群声明无效。");
    validTarget(cluster.cluster_id);
    if (!Array.isArray(cluster.services) || !cluster.services.length || cluster.services.some((service, index) => !["peer", "client"].includes(service) || index && ["peer", "client"].indexOf(cluster.services[index - 1]) >= ["peer", "client"].indexOf(service))) throw new Error("公告服务声明无效。");
  }
  const now = Math.floor(Date.now() / 1000);
  if (!Number.isSafeInteger(value.issued_at) || !Number.isSafeInteger(value.expires_at) || value.issued_at > now + 30 || value.expires_at <= now || value.expires_at <= value.issued_at || value.expires_at > value.issued_at + 180) throw new Error("集群公告已过期或有效期无效。");
  for (const urls of [value.direct_urls, value.relay_urls]) {
    if (!Array.isArray(urls) || urls.length > 8 || urls.some(url => typeof url !== "string")) throw new Error("公告地址无效。");
    for (const url of urls) networkUrl(url);
  }
  if (typeof value.signature !== "string" || !/^[a-f0-9]{128}$/.test(value.signature)) throw new Error("公告签名无效。");
  verify(publicKey, "reachability-v2", encoder.encode(canonical({ ...value, signature: "" })), value.signature);
  return value;
}

async function clusterRecord(value, clusterId) {
  await verifyReachability(value);
  if (value.cluster?.cluster_id !== clusterId || !value.cluster.services.includes("client")) throw new Error("公告不属于目标集群 Gateway。");
  const { peer_id: peerId, public_key: publicKey } = value;
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

export async function discoverCluster(target, { signal, trustedPeers = [], attempt = 0 } = {}) {
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
      candidate = { peerId: record.peerId, publicKey: record.publicKey, expiresAt: record.expiresAt, paths: [] };
      peers.set(record.peerId, candidate);
    }
    candidate.expiresAt = Math.min(candidate.expiresAt, record.expiresAt);
    const directory = new URL(sources[source]);
    if (["http:", "https:"].includes(directory.protocol)) { directory.pathname = "/api/network/ws"; directory.protocol = directory.protocol === "https:" ? "wss:" : "ws:"; }
    const paths = [{ address: directory.href, mode: "relay" }];
    if (trustedPeers.includes(record.peerId)) paths.push(...record.direct.filter(canConnect).map(address => ({ address: networkUrl(address), mode: "direct" })));
    for (const path of paths) if (!candidate.paths.some(item => item.address === path.address && item.mode === path.mode)) candidate.paths.push(path);
  }
  return [...peers.values()].map(candidate => {
    const start = attempt % candidate.paths.length;
    return { ...candidate, paths: [...candidate.paths.slice(start), ...candidate.paths.slice(0, start)].slice(0, 2) };
  });
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

function networkUrl(value) {
  const url = new URL(value);
  if (value.length > 2048 || !["ws:", "wss:"].includes(url.protocol) || !url.hostname || url.username || url.password || url.search || url.hash) throw new Error("设备提供的连接地址无效或不安全。");
  return url.href;
}

function canConnect(value) {
  const url = new URL(value);
  return location.protocol !== "https:" || url.protocol === "wss:" || ["localhost", "127.0.0.1", "[::1]"].includes(url.hostname);
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
    await verifyReachability(record);
    if (record.peer_id !== target) throw new Error("设备地址记录与目标 Peer ID 不符。");
    return [
      ...record.direct_urls.map(value => ({ url: networkUrl(value), relay: false, expires: record.expires_at })),
      ...record.relay_urls.map(value => ({ url: networkUrl(value), relay: true, expires: record.expires_at })),
    ].filter(candidate => canConnect(candidate.url));
  } finally { wire.close(); }
}

export async function connectDevice(entry, { onEvent, onClose, signal } = {}) {
  if (signal?.aborted) throw new Error("设备连接已取消。");
  if (!identity) await initClient();
  if (signal?.aborted) throw new Error("设备连接已取消。");
  if (Object.hasOwn(entry, "relay")) throw new Error("连接入口字段无效。请使用地址和 Peer ID。");
  const target = validTarget(entry.peer_id);
  const endpoint = validEndpoint(entry.address);
  if (entry.local === true) {
    if (endpoint !== location.origin) throw new Error("本机会话只能连接当前本机入口。");
    const local = new URL("/api/local/ws", endpoint);
    local.protocol = local.protocol === "https:" ? "wss:" : "ws:";
    return connectPath(local.href, target, null, { onEvent, onClose, signal });
  }
  const initial = new URL(endpoint);
  if (["http:", "https:"].includes(initial.protocol)) { initial.pathname = "/api/network/ws"; initial.protocol = initial.protocol === "https:" ? "wss:" : "ws:"; }
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
          if (candidate.url === entryUrl && candidate.relay) add({ url: entryUrl, relay: false });
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

export async function connectClusterPath(path, options = {}) {
  if (options.signal?.aborted) throw new Error("设备连接已取消。");
  if (!identity) await initClient();
  if (options.signal?.aborted) throw new Error("设备连接已取消。");
  const target = validTarget(path.peer_id);
  if (path.local === true || !["direct", "relay"].includes(path.mode)) throw new Error("集群路径模式无效。");
  const url = networkUrl(validEndpoint(path.address));
  return connectPath(url, target, path.mode === "relay", options);
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
