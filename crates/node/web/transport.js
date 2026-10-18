import init, { prepareGatewayOwnerRoute, prepareGatewayClientReply, Identity, Handshake, signClientInvocation, signForwardedClientInvocation, signClientPrepareIntent, prepareAccountCreation, prepareAccountRecovery, buildRecoveryContextRequest, createDeviceEnrollmentOffer, createDeviceEnrollmentIntentFromOffer, signDeviceEnrollmentIntent, signDeviceEnrollmentPossession, prepareAccountDeviceEnrollment, prepareAccountDeviceRevocation, signPasskeyEnrollmentIntentFromOffer, signPasskeyEnrollmentPossession, preparePasskeyDeviceEnrollmentFromOffer, buildFirstAuthenticatorEnrollmentIntent, prepareFirstAuthenticatorRegistration, prepareAccountNodeEnrollment, prepareTeamCreation, restorePreparedTeamCreation, prepareSpaceMembershipChange, prepareDirectionalGrant, prepareSpaceGrantRevocation, buildCrossNodeRecoveryRequest, prepareCrossNodeAccountRecovery, restorePreparedCrossNodeRecoveryCommit, prepareExecutionServiceRegistration, prepareRemoteExecutionShare, buildClientPolicyReadRequest as nativeBuildClientPolicyReadRequest, beginClientPolicyImport as nativeBeginClientPolicyImport, verifyClientPolicySelections as nativeVerifyClientPolicySelections, buildClientPolicyAuthConfirmRequest as nativeBuildClientPolicyAuthConfirmRequest, buildAccountRecoveryPending, restoreAccountRecoveryPending, buildRemoteShareIntent, buildRemoteSharePolicyReadRequest, prepareAccountNodeEnrollmentStatus, buildEnrollmentOfferPolicyReadRequest, beginEnrollmentOfferImport, buildRemoteExecutionPolicyReadIntent, buildRemoteExecutionHistoryPolicyReadIntent } from "./generated/vibeface_secure.js";

const encoder = new TextEncoder();
const decoder = new TextDecoder("utf-8", { fatal: true });
const nodePattern = /^[a-f0-9]{64}$/;
const vaultFormat = "vibeface-device-vault-v3";
const prfVaultFormat = "vibeface-device-prf-v3";
const vaultIterations = 600000;
const vaultMarker = "vibeface.v3.device-created";
let identity;
let initializing;
let identityGeneration = 0;
let activeProfileId = null;
let localNodeId = null;
const pendingIdentities = new Map();
const pendingVaultWrites = new Set();

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
    content = fragment.get("enroll") || fragment.get("invite") || fragment.get("bootstrap") || "";
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
  if (!value || typeof value !== "object" || Array.isArray(value) || value.format !== "vibeface-device-profile-v3" || typeof value.id !== "string" || !/^[a-f0-9-]{36}$/.test(value.id) || typeof value.name !== "string" || !value.name.trim() || encoder.encode(value.name).length > 128 || /[\u0000-\u001f\u007f]/.test(value.name) || !["format,id,name,public_key,vault", "format,id,name,passkey,public_key,vault"].includes(Object.keys(value).sort().join(","))) throw new Error("加密设备配置格式无效。旧版仓库不会导入。");
  unhex(value.public_key, 32);
  parseVault(value.vault);
  if (Object.hasOwn(value, "passkey")) parsePrfVault(value.passkey);
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

function base64url(bytes) { return btoa(String.fromCharCode(...bytes)).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, ""); }
function parseCredentialId(value) {
  if (typeof value !== "string" || !/^[A-Za-z0-9_-]+$/.test(value) || value.length > 1366) throw new Error("本地 Passkey 凭证编号无效。");
  let bytes;
  try { bytes = Uint8Array.from(atob(value.replace(/-/g, "+").replace(/_/g, "/")), character => character.charCodeAt(0)); }
  catch { throw new Error("本地 Passkey 凭证编号无效。"); }
  if (!bytes.length || bytes.length > 1024 || base64url(bytes) !== value) throw new Error("本地 Passkey 凭证编号无效。");
}
function parsePrfVault(value) {
  if (!value || typeof value !== "object" || Array.isArray(value) || Object.keys(value).sort().join(",") !== "cipher,ciphertext,credential_id,format,iv,kdf,origin,rp_id,salt" || value.format !== prfVaultFormat || value.kdf !== "HKDF-SHA256" || value.cipher !== "AES-256-GCM") throw new Error("本地 Passkey 保护格式无效。");
  parseCredentialId(value.credential_id);
  let origin;
  try { origin = new URL(value.origin); } catch { throw new Error("本地 Passkey 站点格式无效。"); }
  if (origin.origin !== value.origin || origin.hostname !== value.rp_id || !["https:", "http:"].includes(origin.protocol) || origin.username || origin.password || origin.search || origin.hash || origin.pathname !== "/") throw new Error("本地 Passkey 站点格式无效。");
  return { salt: unhex(value.salt, 32), iv: unhex(value.iv, 12), ciphertext: unhex(value.ciphertext, 48) };
}
function prfAdditionalData(profile, value) {
  return encoder.encode(JSON.stringify({ format: value.format, kdf: value.kdf, cipher: value.cipher, credential_id: value.credential_id, origin: value.origin, rp_id: value.rp_id, salt: value.salt, profile_id: profile.id, public_key: profile.public_key }));
}
async function prfVaultKey(prf, salt, usage) {
  if (!(prf instanceof Uint8Array) || prf.length !== 32) throw new Error("Passkey 本地保护密钥必须恰好为 32 字节。");
  const material = await crypto.subtle.importKey("raw", prf, "HKDF", false, ["deriveKey"]);
  return crypto.subtle.deriveKey({ name: "HKDF", hash: "SHA-256", salt, info: encoder.encode(prfVaultFormat) }, material, { name: "AES-GCM", length: 256 }, false, [usage]);
}
async function sealPrfVault(profile, secret, options) {
  let cleartext;
  try {
    parseCredentialId(options.credentialId);
    if (!(options.salt instanceof Uint8Array) || options.salt.length !== 32) throw new Error("本地 Passkey 保护盐必须恰好为 32 字节。");
    const value = { format: prfVaultFormat, kdf: "HKDF-SHA256", cipher: "AES-256-GCM", credential_id: options.credentialId, origin: location.origin, rp_id: location.hostname, salt: hex(options.salt), iv: hex(crypto.getRandomValues(new Uint8Array(12))) };
    const key = await prfVaultKey(options.prf, options.salt, "encrypt");
    cleartext = unhex(secret, 32);
    const ciphertext = await crypto.subtle.encrypt({ name: "AES-GCM", iv: unhex(value.iv, 12), additionalData: prfAdditionalData(profile, value) }, key, cleartext);
    return { ...value, ciphertext: hex(new Uint8Array(ciphertext)) };
  } finally { cleartext?.fill(0); if (options?.prf instanceof Uint8Array) options.prf.fill(0); }
}
async function openPrfVault(profile, prf) {
  let cleartext;
  try {
    const value = profile.passkey;
    const { salt, iv, ciphertext } = parsePrfVault(value);
    if (value.origin !== location.origin || value.rp_id !== location.hostname) throw new Error("本地 Passkey 属于其他站点。请明确选择独立口令解锁。");
    const key = await prfVaultKey(prf, salt, "decrypt");
    cleartext = new Uint8Array(await crypto.subtle.decrypt({ name: "AES-GCM", iv, additionalData: prfAdditionalData(profile, value) }, key, ciphertext));
    if (cleartext.length !== 32) throw new Error("invalid key length");
    return hex(cleartext);
  } catch { throw new Error("无法使用本地 Passkey 解锁。请保留设备配置，或明确选择独立口令解锁。"); }
  finally { cleartext?.fill(0); if (prf instanceof Uint8Array) prf.fill(0); }
}

export async function passkeyVaultUnlockOptions(profileId) {
  requireSecureClient();
  if (typeof profileId !== "string" || !/^[a-f0-9-]{36}$/.test(profileId)) throw new Error("请选择有效设备配置。");
  const profile = parseProfile(await vaultRecords(profileId));
  if (profile.id !== profileId) throw new Error("设备配置编号不一致。");
  const { salt } = parsePrfVault(profile.passkey);
  if (profile.passkey.origin !== location.origin || profile.passkey.rp_id !== location.hostname) throw new Error("本地 Passkey 属于其他站点。请明确选择独立口令解锁。");
  return { credentialId: profile.passkey.credential_id, salt, options: { publicKey: { challenge: base64url(crypto.getRandomValues(new Uint8Array(32))), rpId: profile.passkey.rp_id, userVerification: "required", allowCredentials: [{ type: "public-key", id: profile.passkey.credential_id }] } } };
}

export async function protectClientVaultWithPrf(options, signal) {
  try {
    signal?.throwIfAborted();
    requireSecureClient();
    if (!identity || !activeProfileId) throw new Error("请先使用独立口令解锁本地设备配置，再启用 Passkey 保护。");
    if (!globalThis.navigator?.locks?.request) throw new Error("此浏览器不支持安全保存本地 Passkey 保护。独立口令仓库保持可用。");
    const profileId = activeProfileId, generation = identityGeneration, own = identity;
    const write = async () => {
      signal?.throwIfAborted();
      const previous = parseProfile(await vaultRecords(profileId));
      signal?.throwIfAborted();
      if (generation !== identityGeneration || identity !== own || previous.id !== profileId || previous.public_key !== own.publicKeyHex()) throw new Error("设备配置已改变，未保存本地 Passkey 保护。");
      const profile = { ...previous, passkey: await sealPrfVault(previous, own.secretHex(), options) };
      signal?.throwIfAborted();
      parseProfile(profile);
      const db = await database();
      try {
        signal?.throwIfAborted();
        await new Promise((resolve, reject) => {
          const transaction = db.transaction("keys", "readwrite"), keys = transaction.objectStore("keys");
          const cancel = () => { try { transaction.abort(); } catch {} };
          signal?.addEventListener("abort", cancel, { once: true });
          pendingVaultWrites.add(transaction);
          const request = keys.get(profileId);
          request.onsuccess = () => {
            try {
              const current = parseProfile(request.result);
              if (signal?.aborted || generation !== identityGeneration || identity !== own || activeProfileId !== profileId || current.id !== profileId || JSON.stringify(current) !== JSON.stringify(previous)) throw new Error("设备配置已改变。");
              keys.put(profile, profileId);
            } catch { transaction.abort(); }
          };
          transaction.oncomplete = () => {
            signal?.removeEventListener("abort", cancel);
            pendingVaultWrites.delete(transaction);
            if (signal?.aborted || generation !== identityGeneration || identity !== own || activeProfileId !== profileId) reject(new Error("设备配置已改变，保存操作已取消。"));
            else resolve();
          };
          transaction.onerror = () => { signal?.removeEventListener("abort", cancel); pendingVaultWrites.delete(transaction); reject(new Error("无法保存本地 Passkey 保护。独立口令仓库保持可用。")); };
          transaction.onabort = transaction.onerror;
        });
      } finally { db.close(); }
    };
    if (signal) await navigator.locks.request(`${vaultFormat}:${profileId}:writer`, { signal }, write);
    else await navigator.locks.request(`${vaultFormat}:${profileId}:writer`, write);
  } finally { if (options?.prf instanceof Uint8Array) options.prf.fill(0); }
}

export async function clientVaultStatus() {
  requireSecureClient();
  await init();
  if (!globalThis.indexedDB) return { status: "unavailable", profiles: [], error: "浏览器不支持加密仓库存储。可以明确选择仅在当前页面保留密钥的临时会话。" };
  let profiles;
  try {
    const records = await vaultRecords();
    profiles = records.map(record => { const profile = parseProfile(record); return { id: profile.id, name: profile.name, passkey: Object.hasOwn(profile, "passkey") }; });
  } catch { return { status: "unavailable", profiles: [], error: "无法读取加密设备仓库。请保留浏览器数据并检查站点存储权限，或明确选择临时会话。" }; }
  let created = false;
  try { created = localStorage.getItem(vaultMarker) === "true"; } catch {}
  return { status: profiles.length ? "locked" : created ? "key_lost" : "empty", profiles };
}

export function lockClient() {
  identityGeneration += 1;
  for (const transaction of pendingVaultWrites) { try { transaction.abort(); } catch {} }
  pendingVaultWrites.clear();
  identity?.free();
  for (const candidate of pendingIdentities.values()) candidate.free();
  pendingIdentities.clear();
  identity = undefined;
  activeProfileId = null;
  initializing = undefined;
}

export function setLocalAccessNode(nodeId) {
  localNodeId = nodeId === null ? null : validTarget(nodeId);
}

function localVaultRecord(key, value, generation = identityGeneration, activeNode, signal) {
  return new Promise((resolve, reject) => {
    if (signal?.aborted) { reject(new Error("本机自动解锁配置已取消。")); return; }
    const opening = indexedDB.open("vibeface-local-vault-v1", 1);
    opening.onupgradeneeded = () => opening.result.createObjectStore("keys");
    opening.onerror = () => reject(new Error("无法读取本机自动解锁配置。"));
    opening.onsuccess = () => {
      const db = opening.result;
      if (generation !== identityGeneration || signal?.aborted) { db.close(); reject(new Error("设备配置已锁定或操作已取消。")); return; }
      const transaction = db.transaction("keys", value ? "readwrite" : "readonly");
      const abort = () => { try { transaction.abort(); } catch {} };
      const close = () => { signal?.removeEventListener("abort", abort); pendingVaultWrites.delete(transaction); db.close(); };
      signal?.addEventListener("abort", abort, { once: true });
      if (value) pendingVaultWrites.add(transaction);
      const request = value ? transaction.objectStore("keys").put(value, key) : transaction.objectStore("keys").get(key);
      if (activeNode) transaction.objectStore("keys").put(value.profileId, `active:${activeNode}`);
      transaction.oncomplete = () => { close(); if (generation !== identityGeneration || signal?.aborted) reject(new Error("设备配置已锁定或操作已取消。")); else resolve(request.result); };
      transaction.onerror = transaction.onabort = () => { close(); reject(new Error("本机自动解锁配置未保存。")); };
    };
  });
}

export async function bindLocalVault(nodeId, unlockKey, isCurrent = () => true, signal) {
  requireSecureClient(); validTarget(nodeId); unhex(unlockKey, 32);
  if (!isCurrent() || signal?.aborted) throw new Error("本机自动解锁配置已取消。");
  if (!identity || !activeProfileId) throw new Error("请先解锁已保存的设备配置。临时会话不能自动解锁。");
  const generation = identityGeneration;
  const profileId = activeProfileId;
  const publicKey = identity.publicKeyHex();
  const key = `${nodeId}:${profileId}`;
  const keyTag = hex(new Uint8Array(await crypto.subtle.digest("SHA-256", encoder.encode(unlockKey))));
  const previous = await localVaultRecord(key, undefined, generation, undefined, signal);
  if (generation !== identityGeneration || profileId !== activeProfileId || !isCurrent() || signal?.aborted) throw new Error("设备配置已锁定或操作已取消。");
  const vault = previous?.nodeId === nodeId && previous?.profileId === profileId && previous?.publicKey === publicKey && previous?.keyTag === keyTag ? previous.vault : await sealDeviceVault(identity.secretHex(), unlockKey);
  if (generation !== identityGeneration || profileId !== activeProfileId || !isCurrent() || signal?.aborted) throw new Error("设备配置已锁定或操作已取消。");
  await localVaultRecord(key, { nodeId, profileId, publicKey, keyTag, vault }, generation, nodeId, signal);
}

export async function unlockLocalVault(nodeId, unlockKey, isCurrent = () => true) {
  requireSecureClient(); validTarget(nodeId); unhex(unlockKey, 32);
  if (identity || initializing) throw new Error("设备配置正在使用。");
  const generation = identityGeneration;
  const profileId = await localVaultRecord(`active:${nodeId}`);
  if (!profileId) return null;
  if (typeof profileId !== "string" || !/^[a-f0-9-]{36}$/.test(profileId)) throw new Error("本机设备配置编号无效。");
  const stored = await localVaultRecord(`${nodeId}:${profileId}`);
  const profile = parseProfile(await vaultRecords(profileId));
  if (!stored || stored.nodeId !== nodeId || stored.profileId !== profileId || stored.publicKey !== profile.public_key) throw new Error("本机自动解锁与原设备配置不一致。");
  await init();
  const secret = await openDeviceVault(stored.vault, unlockKey);
  if (generation !== identityGeneration || identity || !isCurrent()) throw new Error("设备配置已切换。");
  const candidate = Identity.fromSecretHex(secret);
  if (candidate.publicKeyHex() !== profile.public_key) { candidate.free(); throw new Error("本机自动解锁密钥不一致。"); }
  identity = candidate; activeProfileId = profileId;
  return { nodeId: identity.nodeId(), publicKey: identity.publicKeyHex(), profileId };
}

export async function initClient(options) {
  try {
    requireSecureClient();
    if (identity) throw new Error("请先锁定当前设备配置，再解锁或创建其他账户的设备配置。");
    if (!options || !["unlock", "unlock_prf", "create", "temporary", "restore"].includes(options.mode)) throw new Error("请先解锁加密仓库，或明确选择临时会话。");
    if (initializing) throw new Error("设备仓库正在处理，请等待完成。");
    const generation = identityGeneration;
    const work = initializeIdentity(options, generation);
    initializing = work;
    try { return await work; } finally { if (initializing === work) initializing = undefined; }
  } finally { if (options?.prf instanceof Uint8Array) options.prf.fill(0); }
}

async function initializeIdentity(options, generation) {
  let candidate;
  let profile = null;
  try {
    await init();
    if (generation !== identityGeneration) throw new Error("设备仓库操作已取消。");
    if (options.mode === "temporary") { candidate = Identity.generate(); pendingIdentities.set(generation, candidate); }
    else {
      if (options.mode === "unlock" || options.mode === "unlock_prf") {
        if (typeof options.profileId !== "string" || !/^[a-f0-9-]{36}$/.test(options.profileId)) throw new Error("请选择有效设备配置。");
        const stored = await vaultRecords(options.profileId);
        if (generation !== identityGeneration) throw new Error("设备仓库操作已取消。");
        if (!stored) throw new Error("此浏览器没有所选设备配置。请恢复备份，或明确创建新设备。");
        profile = parseProfile(stored);
        if (profile.id !== options.profileId) throw new Error("设备配置编号不一致。");
        const secret = options.mode === "unlock_prf" ? await openPrfVault(profile, options.prf) : await openDeviceVault(profile.vault, options.password);
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

export function prepareClientGatewayOwnerRoute(materialJson, ownerAuthorization, payloadJson) {
  if (!identity) throw new Error("请先解锁当前账户的设备配置。");
  return prepareGatewayOwnerRoute(materialJson, ownerAuthorization, payloadJson, identity, Date.now());
}

export function prepareClientGatewayReply(authorization, originalJson, routeHash, gatewayPublicKey) {
  if (!identity) throw new Error("请先解锁当前账户的设备配置。");
  return prepareGatewayClientReply(authorization, originalJson, routeHash, gatewayPublicKey, identity, Date.now());
}

export function signAuthorizedClientInvocation(authorization, templateJson, bodyJson, expectedAction, operationId, gatewayPublicKey) {
  if (!identity) throw new Error("请先解锁当前账户的设备配置。");
  if (gatewayPublicKey) return signForwardedClientInvocation(authorization, templateJson, bodyJson, expectedAction, operationId, gatewayPublicKey, identity, Date.now());
  return signClientInvocation(authorization, templateJson, bodyJson, expectedAction, operationId, identity, Date.now());
}

export function signAuthorizedClientPrepare(authorization, bodyJson, contextJson, expectedAction, operationId, gatewayPublicKey) {
  if (!identity) throw new Error("请先解锁当前账户的设备配置。");
  return signClientPrepareIntent(authorization, bodyJson, contextJson, expectedAction, operationId, gatewayPublicKey, identity, Date.now());
}

export function prepareRemoteClientExecution(remote, authorization, bodyJson, contextJson, expectedAction, operationId, gatewayPublicKey) {
  if (!identity) throw new Error("请先解锁当前账户的设备配置。");
  return remote.prepare(authorization, bodyJson, contextJson, expectedAction, operationId, gatewayPublicKey, identity, Date.now());
}

export function signRemoteClientExecution(remote, authorization, templateJson, bodyJson, expectedAction, operationId, gatewayPublicKey) {
  if (!identity) throw new Error("请先解锁当前账户的设备配置。");
  return remote.sign(authorization, templateJson, bodyJson, expectedAction, operationId, gatewayPublicKey, identity, Date.now());
}

export function buildClientCrossNodeRecoveryRequest(materialJson, nextRoot, claim, resourceJson, readExpiresAt, requestExpiresAt) {
  if (!identity) throw new Error("请先解锁新的独立设备配置。");
  return buildCrossNodeRecoveryRequest(materialJson, nextRoot, identity, claim, resourceJson, readExpiresAt, Date.now(), requestExpiresAt);
}

export function prepareClientCrossNodeAccountRecovery(materialJson, context, nextRoot, certificateExpiresAt) {
  if (!identity) throw new Error("请先解锁新的独立设备配置。");
  return prepareCrossNodeAccountRecovery(materialJson, context, nextRoot, identity, certificateExpiresAt, Date.now());
}

export function restoreClientCrossNodeRecovery(materialJson) {
  if (!identity) throw new Error("请先解锁原恢复设备配置后继续。");
  const prepared = restorePreparedCrossNodeRecoveryCommit(materialJson, Date.now());
  try {
    const client = prepared.clientIdentity();
    try {
      if (client.publicKeyHex() !== identity.publicKeyHex()) throw new Error("请解锁原恢复设备配置后继续。");
      return prepared;
    } finally { client.free(); }
  } catch (error) { prepared.free(); throw error; }
}

export function prepareClientAccountCreation(root, recovery, claim, certificateExpiresAt) {
  if (!identity) throw new Error("请先解锁当前账户的设备配置。");
  return prepareAccountCreation(root, recovery, identity, claim, certificateExpiresAt, Date.now());
}

export function prepareClientAccountRecovery(materialJson, context, nextRoot, certificateExpiresAt) {
  if (!identity) throw new Error("请先解锁新的独立设备配置。");
  return prepareAccountRecovery(materialJson, context, nextRoot, identity, certificateExpiresAt, Date.now());
}

export function buildClientAccountRecoveryPending(prepared, created, nextRoot, previousMaterialJson, endpoint) {
  if (!identity) throw new Error("请先解锁原恢复设备配置。");
  return buildAccountRecoveryPending(prepared, created, nextRoot, identity, previousMaterialJson, endpoint, Date.now());
}

export function restoreClientAccountRecovery(pendingJson) {
  if (!identity) throw new Error("请先解锁原恢复设备配置。");
  const pending = restoreAccountRecoveryPending(pendingJson, Date.now());
  try {
    const client = pending.clientIdentity();
    try {
      if (client.publicKeyHex() !== identity.publicKeyHex()) throw new Error("请解锁原恢复设备配置后继续。");
      return pending;
    } finally { client.free(); }
  } catch (error) { pending.free(); throw error; }
}

export function prepareClientRecoveryContext(materialJson, expectedNodePublicKey) {
  if (!identity) throw new Error("请先解锁新的独立设备配置。");
  const now = Date.now();
  return buildRecoveryContextRequest(materialJson, identity, expectedNodePublicKey, now, now + 120000);
}

export function createClientEnrollmentOffer(accountId, endpoint, nodePublicKey, requestedRightsJson, certificateExpiresAt, authorization) {
  if (!identity) throw new Error("请先解锁当前账户的设备配置。");
  return createDeviceEnrollmentOffer(identity, accountId, endpoint, nodePublicKey, requestedRightsJson, certificateExpiresAt, authorization, Date.now());
}

export function createClientEnrollmentIntent(offer) {
  if (!identity) throw new Error("请先解锁新设备配置。");
  return createDeviceEnrollmentIntentFromOffer(offer, identity, Date.now());
}

export function approveClientEnrollmentIntent(intentJson, authorization) {
  if (!identity) throw new Error("请先解锁当前账户的设备配置。");
  return signDeviceEnrollmentIntent(intentJson, identity, authorization, Date.now());
}

export function signClientEnrollmentPossession(qualified, pendingNonce) {
  if (!identity) throw new Error("请先解锁新设备配置。");
  return signDeviceEnrollmentPossession(qualified, identity, pendingNonce, Date.now());
}

export function prepareClientDeviceEnrollment(qualified, possessionJson, authorization) {
  if (!identity) throw new Error("请先解锁当前账户的设备配置。");
  return prepareAccountDeviceEnrollment(qualified, possessionJson, identity, authorization, Date.now());
}

export function prepareClientDeviceRevocation(accountId, deviceId, authorization) {
  if (!identity) throw new Error("请先解锁当前账户的设备配置。");
  return prepareAccountDeviceRevocation(accountId, deviceId, identity, authorization, Date.now());
}

export function signClientPasskeyEnrollmentIntent(intentJson, offer) {
  if (!identity) throw new Error("请先解锁新设备配置。");
  return signPasskeyEnrollmentIntentFromOffer(intentJson, identity, offer, Date.now());
}

export function signClientPasskeyEnrollmentPossession(qualified, pendingNonce) {
  if (!identity) throw new Error("请先解锁新设备配置。");
  return signPasskeyEnrollmentPossession(qualified, identity, pendingNonce, Date.now());
}

export function prepareClientPasskeyDeviceEnrollment(intentJson, certificateJson, proofJson, offer) {
  if (!identity) throw new Error("请先解锁新设备配置。");
  return preparePasskeyDeviceEnrollmentFromOffer(intentJson, certificateJson, proofJson, identity, offer, Date.now());
}

export function prepareClientFirstAuthenticatorIntent(materialJson, authorization, credentialExpiresAt) {
  if (!identity) throw new Error("请先解锁当前账户的设备配置。");
  return buildFirstAuthenticatorEnrollmentIntent(materialJson, authorization, identity, Date.now(), credentialExpiresAt);
}

export function prepareClientFirstAuthenticatorRegistration(materialJson, authorization, intentJson, registration) {
  if (!identity) throw new Error("请先解锁当前账户的设备配置。");
  return prepareFirstAuthenticatorRegistration(materialJson, authorization, intentJson, registration, Date.now());
}

export function prepareClientAccountNodeEnrollment(materialJson, authorization, claim, certificateExpiresAt) {
  if (!identity) throw new Error("请先解锁当前账户的设备配置。");
  return prepareAccountNodeEnrollment(materialJson, authorization, identity, claim, certificateExpiresAt, Date.now());
}

export function prepareClientTeamCreation(root, ownerAccountId, targetNodePublicKey, recoveryPublicKey, sourceProjectResourceId, authorization, certificateExpiresAt) {
  if (!identity) throw new Error("请先解锁当前账户的设备配置。");
  return prepareTeamCreation(root, identity, ownerAccountId, targetNodePublicKey, recoveryPublicKey, sourceProjectResourceId, authorization, certificateExpiresAt, Date.now());
}

export function restoreClientTeamCreation(materialJson, authorization) {
  if (!identity) throw new Error("请先解锁当前账户的设备配置。");
  return restorePreparedTeamCreation(materialJson, identity, authorization, Date.now());
}

export function prepareClientMembershipChange(intentJson, authorization) {
  if (!identity) throw new Error("请先解锁当前账户的设备配置。");
  return prepareSpaceMembershipChange(intentJson, identity, authorization, Date.now());
}

export function prepareClientDirectionalGrant(intentJson, authorization) {
  if (!identity) throw new Error("请先解锁当前账户的设备配置。");
  return prepareDirectionalGrant(intentJson, identity, authorization, Date.now());
}

export function prepareClientGrantRevocation(spaceId, grantHash, authorization) {
  if (!identity) throw new Error("请先解锁当前账户的设备配置。");
  return prepareSpaceGrantRevocation(spaceId, grantHash, identity, authorization, Date.now());
}

export function buildClientPolicyReadRequest(account, space, nodePublicKey, pinsJson, selectionJson, expiresAt) {
  if (!identity) throw new Error("请先解锁设备配置。");
  return nativeBuildClientPolicyReadRequest(account, space, identity, nodePublicKey, pinsJson, selectionJson, Date.now(), expiresAt);
}

export function beginClientPolicyImport(pinsJson, requestJson, snapshotJson, nodePublicKey) {
  if (!identity) throw new Error("请先解锁设备配置。");
  return nativeBeginClientPolicyImport(pinsJson, requestJson, snapshotJson, identity, nodePublicKey, Date.now());
}

export function verifyClientPolicySelections(pinsJson, requestJson, pageJson, nodePublicKey) {
  if (!identity) throw new Error("请先解锁设备配置。");
  return nativeVerifyClientPolicySelections(pinsJson, requestJson, pageJson, identity, nodePublicKey, Date.now());
}

export function buildClientPolicyAuthConfirmRequest(authorization, expiresAt) {
  if (!identity) throw new Error("请先解锁设备配置。");
  return nativeBuildClientPolicyAuthConfirmRequest(authorization, identity, Date.now(), expiresAt);
}

export function prepareClientExecutionServiceRegistration(source, management, owner, ownerMaterialJson, targetNodePublicKey, expiresAt) {
  if (!identity) throw new Error("请先解锁当前账户的设备配置。");
  return prepareExecutionServiceRegistration(source, management, owner, ownerMaterialJson, identity, targetNodePublicKey, Date.now(), expiresAt);
}

export function buildClientRemoteShareIntent(intentJson, source) {
  if (!identity) throw new Error("请先解锁当前账户的设备配置。");
  return buildRemoteShareIntent(intentJson, source, identity, Date.now());
}

export function buildClientRemoteSharePolicyReadRequest(intentJson, pinsJson, nodePublicKey, expiresAt) {
  if (!identity) throw new Error("请先解锁当前账户的设备配置。");
  return buildRemoteSharePolicyReadRequest(intentJson, identity, pinsJson, nodePublicKey, Date.now(), expiresAt);
}

export function prepareClientRemoteExecutionShare(intentJson, authorization, source) {
  if (!identity) throw new Error("请先解锁当前账户的设备配置。");
  return prepareRemoteExecutionShare(intentJson, identity, authorization, source, Date.now());
}

export function signRemoteExecutionObservation(remote, originalJson, authorizationId, pinnedGatewayPublicKey) {
  if (!identity) throw new Error("请先解锁当前账户的设备配置。");
  return remote.signObservationRequest(originalJson, authorizationId, pinnedGatewayPublicKey, identity, Date.now());
}

export async function connectDevice(entry, { onEvent, onClose, signal } = {}) {
  if (signal?.aborted) throw new Error("设备连接已取消。");
  if (!identity) throw new Error("请先解锁设备仓库，或选择临时会话。");
  const target = validTarget(entry.node_id);
  const endpoint = validEndpoint(entry.address);
  const local = localNodeId === target && endpoint === location.origin && !entry.relay;
  const url = new URL(local ? "/api/local/network/ws" : "/api/network/ws", endpoint);
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
    handshake = new Handshake(true, identity.secretHex(), target, encoder.encode("vibeface:client:v2"));
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

export function prepareClientAccountNodeEnrollmentStatus(prepared, expiresAt) {
  if (!identity) throw new Error("请先解锁原账户设备配置。");
  return prepareAccountNodeEnrollmentStatus(prepared, identity, Date.now(), expiresAt);
}

export function buildClientEnrollmentPolicyReadRequest(offer, expiresAt) {
  if (!identity) throw new Error("请先解锁新的独立设备配置。");
  return buildEnrollmentOfferPolicyReadRequest(offer, identity, Date.now(), expiresAt);
}

export function beginClientEnrollmentOfferImport(offer, requestJson, snapshotJson, nodePublicKey) {
  if (!identity) throw new Error("请先解锁新的独立设备配置。");
  return beginEnrollmentOfferImport(offer, requestJson, snapshotJson, identity, nodePublicKey, Date.now());
}

export function buildClientRemoteExecutionPolicyReadIntent(source, inputJson, expiresAt) {
  if (!identity) throw new Error("请先解锁当前账户的设备配置。");
  return buildRemoteExecutionPolicyReadIntent(source, inputJson, identity, Date.now(), expiresAt);
}

export function buildClientRemoteExecutionHistoryPolicyReadIntent(source, inputJson, independentPinsJson, expiresAt) {
  if (!identity) throw new Error("请先解锁当前账户的设备配置。");
  return buildRemoteExecutionHistoryPolicyReadIntent(source, inputJson, independentPinsJson, identity, Date.now(), expiresAt);
}
