import init, { Identity, Handshake, signClientInvocation } from "/generated/vibeface_secure.js";

const encoder = new TextEncoder();
const decoder = new TextDecoder("utf-8", { fatal: true });
const nodePattern = /^[a-f0-9]{64}$/;
const vaultFormat = "vibeface-device-vault-v3";
const vaultIterations = 600000;
const vaultMarker = "vibeface.v3.device-created";
let identity;
let initializing;
let identityGeneration = 0;
let activeProfileId = null;
const pendingIdentities = new Map();

export function validTarget(value) {
  if (typeof value !== "string" || !nodePattern.test(value)) throw new Error("填写完整的 64 位设备指纹。请从本机终端或可信邀请中取得，不要使用中继提供的替代指纹。");
  return value;
}

export function validEndpoint(value) {
  const url = new URL(value);
  if (!["http:", "https:"].includes(url.protocol) || url.username || url.password || url.search || url.hash || !["", "/"].includes(url.pathname)) throw new Error("填写 HTTP 或 HTTPS 入口地址，不包含路径、账号或密钥。");
  const local = ["localhost", "127.0.0.1", "[::1]"].includes(url.hostname);
  if (location.protocol === "https:" && url.protocol === "http:" && !local) throw new Error("HTTPS 客户端需要 HTTPS 入口。请为目标或中继启用 HTTPS，再保留相同设备指纹连接。");
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
    catch { throw new Error("无法读取邀请。请粘贴完整邀请链接或终端输出的邀请内容。"); }
  }
  if (!invitation || typeof invitation !== "object" || Array.isArray(invitation)) throw new Error("邀请内容格式无效。");
  return invitation;
}

function requireSecureClient() {
  if (!globalThis.isSecureContext || !globalThis.crypto?.subtle || !globalThis.crypto?.getRandomValues) throw new Error("此地址不能安全使用设备密钥。请在本机使用 http://localhost:4790，或打开你信任的 HTTPS 客户端。普通局域网 HTTP 不会降级认证。");
}

function database() {
  if (!globalThis.indexedDB) throw new Error("浏览器不支持加密仓库存储。可以选择仅在当前页面使用的临时会话。");
  return new Promise((resolve, reject) => {
    const request = indexedDB.open(vaultFormat, 1);
    request.onupgradeneeded = () => request.result.createObjectStore("keys");
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(new Error("无法打开加密设备仓库。请允许此站点使用 IndexedDB，或选择临时会话。"));
  });
}

async function vaultRecords(id, write) {
  const db = await database();
  try {
    return await new Promise((resolve, reject) => {
      const transaction = db.transaction("keys", write ? "readwrite" : "readonly");
      const keys = transaction.objectStore("keys");
      const request = write ? keys.add(write, id) : id ? keys.get(id) : keys.getAll();
      let result;
      request.onsuccess = () => { result = request.result; };
      transaction.oncomplete = () => resolve(result);
      transaction.onerror = () => reject(new Error(write ? "无法保存加密设备配置。已有备份不会覆盖，请重新解锁。" : "无法读取加密仓库。请保留浏览器数据并重试。"));
      transaction.onabort = transaction.onerror;
    });
  } finally { db.close(); }
}

function parseProfile(value) {
  if (!value || typeof value !== "object" || Array.isArray(value) || value.format !== "vibeface-device-profile-v3" || typeof value.id !== "string" || !/^[a-f0-9-]{36}$/.test(value.id) || typeof value.name !== "string" || !value.name.trim() || encoder.encode(value.name).length > 128 || /[\u0000-\u001f\u007f]/.test(value.name) || Object.keys(value).sort().join(",") !== "format,id,name,public_key,vault") throw new Error("加密设备配置格式无效。旧版仓库不会导入。");
  unhex(value.public_key, 32);
  parseVault(value.vault);
  return value;
}

function passwordBytes(password, creating = false) {
  if (typeof password !== "string" || !password || encoder.encode(password).length > 1024 || creating && [...password].length < 12) throw new Error("加密仓库口令需要至少 12 个字符，且不超过 1024 字节。");
  return encoder.encode(password);
}

function hex(bytes) { return [...bytes].map(byte => byte.toString(16).padStart(2, "0")).join(""); }
function unhex(value, length) {
  if (typeof value !== "string" || !/^[a-f0-9]+$/.test(value) || value.length !== length * 2) throw new Error("加密仓库格式无效。旧版明文设备仓库不会导入。");
  return Uint8Array.from(value.match(/../g), byte => Number.parseInt(byte, 16));
}
function parseVault(value) {
  if (!value || typeof value !== "object" || Array.isArray(value) || value.format !== vaultFormat || value.kdf !== "PBKDF2-SHA256" || value.iterations !== vaultIterations || value.cipher !== "AES-256-GCM" || Object.keys(value).sort().join(",") !== "cipher,ciphertext,format,iterations,iv,kdf,salt") throw new Error("加密仓库版本无效。旧版仓库不会导入。");
  return { salt: unhex(value.salt, 16), iv: unhex(value.iv, 12), ciphertext: unhex(value.ciphertext, 48) };
}
async function vaultKey(password, salt, usage, creating = false) {
  const bytes = passwordBytes(password, creating);
  try {
    const material = await crypto.subtle.importKey("raw", bytes, "PBKDF2", false, ["deriveKey"]);
    return await crypto.subtle.deriveKey({ name: "PBKDF2", hash: "SHA-256", salt, iterations: vaultIterations }, material, { name: "AES-GCM", length: 256 }, false, [usage]);
  } finally { bytes.fill(0); }
}

export async function sealDeviceVault(secret, password) {
  const cleartext = unhex(secret, 32);
  const salt = crypto.getRandomValues(new Uint8Array(16));
  const iv = crypto.getRandomValues(new Uint8Array(12));
  try {
    const key = await vaultKey(password, salt, "encrypt", true);
    const ciphertext = await crypto.subtle.encrypt({ name: "AES-GCM", iv, additionalData: encoder.encode(vaultFormat) }, key, cleartext);
    return { format: vaultFormat, kdf: "PBKDF2-SHA256", iterations: vaultIterations, cipher: "AES-256-GCM", salt: hex(salt), iv: hex(iv), ciphertext: hex(new Uint8Array(ciphertext)) };
  } finally { cleartext.fill(0); }
}

export async function openDeviceVault(value, password) {
  const { salt, iv, ciphertext } = parseVault(value);
  let cleartext;
  try {
    const key = await vaultKey(password, salt, "decrypt");
    cleartext = new Uint8Array(await crypto.subtle.decrypt({ name: "AES-GCM", iv, additionalData: encoder.encode(vaultFormat) }, key, ciphertext));
    if (cleartext.length !== 32) throw new Error("invalid key length");
    return hex(cleartext);
  } catch { throw new Error("无法解锁设备仓库。请核对口令，并使用未修改的备份。"); }
  finally { cleartext?.fill(0); }
}

export async function clientVaultStatus() {
  requireSecureClient();
  await init();
  if (!globalThis.indexedDB) return { status: "unavailable", profiles: [], error: "浏览器不支持加密仓库存储。可以明确选择仅在当前页面保留密钥的临时会话。" };
  let profiles;
  try {
    const records = await vaultRecords();
    profiles = records.map(record => { const profile = parseProfile(record); return { id: profile.id, name: profile.name }; });
  } catch { return { status: "unavailable", profiles: [], error: "无法读取加密设备仓库。请保留浏览器数据并检查站点存储权限，或明确选择临时会话。" }; }
  let created = false;
  try { created = localStorage.getItem(vaultMarker) === "true"; } catch {}
  return { status: profiles.length ? "locked" : created ? "key_lost" : "empty", profiles };
}

export function lockClient() {
  identityGeneration += 1;
  identity?.free();
  for (const candidate of pendingIdentities.values()) candidate.free();
  pendingIdentities.clear();
  identity = undefined;
  activeProfileId = null;
  initializing = undefined;
}

export async function initClient(options) {
  requireSecureClient();
  if (identity) throw new Error("请先锁定当前设备配置，再解锁或创建其他账户的设备配置。");
  if (!options || !["unlock", "create", "temporary", "restore"].includes(options.mode)) throw new Error("请先解锁加密仓库，或明确选择临时会话。");
  if (initializing) throw new Error("设备仓库正在处理，请等待完成。");
  const generation = identityGeneration;
  const work = initializeIdentity(options, generation);
  initializing = work;
  try { return await work; } finally { if (initializing === work) initializing = undefined; }
}

async function initializeIdentity(options, generation) {
  let candidate;
  let profile = null;
  try {
    await init();
    if (generation !== identityGeneration) throw new Error("设备仓库操作已取消。");
    if (options.mode === "temporary") { candidate = Identity.generate(); pendingIdentities.set(generation, candidate); }
    else {
      if (options.mode === "unlock") {
        if (typeof options.profileId !== "string" || !/^[a-f0-9-]{36}$/.test(options.profileId)) throw new Error("请选择有效设备配置。");
        const stored = await vaultRecords(options.profileId);
        if (generation !== identityGeneration) throw new Error("设备仓库操作已取消。");
        if (!stored) throw new Error("此浏览器没有所选设备配置。请恢复备份，或明确创建新设备。");
        profile = parseProfile(stored);
        const secret = await openDeviceVault(profile.vault, options.password);
        if (generation !== identityGeneration) throw new Error("设备仓库操作已取消。");
        candidate = Identity.fromSecretHex(secret);
        pendingIdentities.set(generation, candidate);
        if (candidate.publicKeyHex() !== profile.public_key) throw new Error("设备备份公钥与加密密钥不一致。");
      } else {
        if (options.mode === "restore") {
          profile = parseProfile(options.vault);
          const secret = await openDeviceVault(profile.vault, options.password);
          if (generation !== identityGeneration) throw new Error("设备仓库操作已取消。");
          candidate = Identity.fromSecretHex(secret);
          pendingIdentities.set(generation, candidate);
          if (candidate.publicKeyHex() !== profile.public_key) throw new Error("设备备份公钥与加密密钥不一致。");
        } else {
          candidate = Identity.generate();
          pendingIdentities.set(generation, candidate);
          profile = { format: "vibeface-device-profile-v3", id: crypto.randomUUID(), name: options.name, public_key: candidate.publicKeyHex(), vault: await sealDeviceVault(candidate.secretHex(), options.password) };
          parseProfile(profile);
        }
        if (generation !== identityGeneration) throw new Error("设备仓库操作已取消。");
        await vaultRecords(profile.id, profile);
        try { localStorage.setItem(vaultMarker, "true"); } catch {}
      }
    }
    if (generation !== identityGeneration) throw new Error("设备仓库操作已取消。");
    identity = candidate;
    pendingIdentities.delete(generation);
    activeProfileId = profile?.id ?? null;
    candidate = undefined;
    return { nodeId: identity.nodeId(), publicKey: identity.publicKeyHex(), profileId: activeProfileId };
  } finally {
    if (candidate && pendingIdentities.get(generation) === candidate) { pendingIdentities.delete(generation); candidate.free(); }
    options.password = "";
  }
}

export async function exportDeviceVault() {
  if (!identity || !activeProfileId) throw new Error("临时会话没有本地设备备份。关闭页面后需要重新批准新设备。");
  return JSON.stringify(parseProfile(await vaultRecords(activeProfileId)));
}

export function signClient(domain, payload) {
  if (!identity) throw new Error("浏览器设备密钥尚未初始化。");
  return identity.sign(domain, encoder.encode(payload));
}

export function signAuthorizedClientInvocation(authorization, templateJson, bodyJson, expectedAction, operationId) {
  if (!identity) throw new Error("请先解锁当前账户的设备配置。");
  return signClientInvocation(authorization, templateJson, bodyJson, expectedAction, operationId, identity, Date.now());
}

export async function connectDevice(entry, { onEvent, onClose, signal } = {}) {
  if (signal?.aborted) throw new Error("设备连接已取消。");
  if (!identity) throw new Error("请先解锁设备仓库，或选择临时会话。");
  const target = validTarget(entry.node_id);
  const endpoint = validEndpoint(entry.address);
  const url = new URL("/api/network/ws", endpoint);
  url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
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
  const handshakeTimer = setTimeout(() => fail(new Error("连接设备超时。请核对设备指纹、地址或中继是否在线。")), 20000);
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
    await control("welcome");
    socket.send(JSON.stringify(entry.relay ? { type: "dial", target, service: "client" } : { type: "direct", service: "client" }));
    await control("ready");
    handshake = new Handshake(true, identity.secretHex(), target, encoder.encode("vibeface:client:v1"));
    socket.send(handshake.write(new Uint8Array()));
    const response = await receive();
    if (!(response instanceof ArrayBuffer)) throw new Error("设备未返回加密握手。");
    if (handshake.read(new Uint8Array(response)).length) throw new Error("设备握手包含未预期的数据。");
    socket.send(handshake.write(new Uint8Array()));
    if (!handshake.isFinished() || handshake.remoteId() !== target) throw new Error("设备身份与已保存指纹不符，已拒绝连接。");
    publicKey = handshake.remotePublicKey();
    if (typeof publicKey !== "string" || !nodePattern.test(publicKey)) throw new Error("设备未提供有效的认证公钥。");
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
    nodeId: target,
    publicKey,
    request(method, params = {}, invocation) {
      if (closed) return Promise.reject(disconnected());
      const id = String(++nextId);
      return new Promise((resolve, reject) => {
        const timer = setTimeout(() => { pending.delete(id); reject(new Error("设备尚未确认请求结果。请核对原操作，不要重复派发。")); }, 60000);
        pending.set(id, { resolve, reject, timer });
        try { for (const record of channel.sealMessage(encoder.encode(JSON.stringify(invocation === undefined ? { id, method, params } : { id, method, params, invocation })))) socket.send(record); }
        catch (error) { fail(error); }
      });
    },
    close() { fail(new Error("已断开设备连接。")); },
  };
}
