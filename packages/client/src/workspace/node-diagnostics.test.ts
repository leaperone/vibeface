import assert from "node:assert/strict"
import test from "node:test"
import type { ConnectionEntry } from "../lib/node.ts"
import { parseNodeReport } from "./model.ts"
import { describeNode, nodeDiagnosticText } from "./node-diagnostics.ts"

const entryId = "a".repeat(64)
const targetId = "b".repeat(64)
const gatewayId = "c".repeat(64)
const networkId = "d".repeat(64)
const checkedAt = 1_800_000_000_000
const lastSyncedAt = checkedAt - 5_000
const entry: ConnectionEntry = { kind: "gateway", device_id: entryId, address: "https://access.example" }
const info = { device_id: entryId, name: "工作电脑", version: "0.0.1", initialized: true, network_id: networkId }
const target = { deviceId: targetId, name: "书房电脑", entry: false }
const peer = {
  device_id: targetId, name: target.name, local: false, online: true, revoked: false,
  capabilities: { relay: true, roots: [], version: "0.0.1", os: "linux", arch: "aarch64" },
}

function inspect({ response, connection = entry, focus = target, connected = true, fresh = true }: {
  response: unknown
  connection?: ConnectionEntry
  focus?: typeof target
  connected?: boolean
  fresh?: boolean
}) {
  const report = { ...parseNodeReport(response), receivedAt: checkedAt }
  return describeNode({
    target: focus, ready: connected ? { entry: connection, info } : null, connected, report, fresh, checking: false,
    clientLabel: "这台手机", nameFor: id => id === gatewayId ? "远程接入机" : undefined,
  })
}

test("a forwarding connection reports its return relay separately from the peer probe route", () => {
  const diagnostics = inspect({ response: {
    session_path: { kind: "quic", upstream_device_id: gatewayId, checked_at_ms: checkedAt + 100, return_path: { kind: "relay", url: "https://return.example/" } },
    nodes: [{ ...peer, observation: { checked_at_ms: checkedAt, last_synced_at_ms: lastSyncedAt, path: { kind: "relay", url: "https://sync.example:443/" } } }],
  } })
  assert.equal(diagnostics.status, "online")
  assert.equal(diagnostics.checkedAt, checkedAt)
  assert.equal(diagnostics.lastSyncedAt, lastSyncedAt)
  assert.equal(diagnostics.system, "Linux · aarch64")
  assert.deepEqual(diagnostics.hops.map(({ kind, name, address }) => [kind, name, address]), [
    ["client", "这台手机", undefined],
    ["gateway", "远程接入机", "https://access.example"],
    ["node", info.name, undefined],
    ["relay", "转发服务", "https://sync.example"],
    ["node", target.name, undefined],
  ])
  assert.deepEqual(diagnostics.returnPath, {
    from: info.name, to: "远程接入机", path: { kind: "relay", url: "https://return.example" }, checkedAt: checkedAt + 100,
  })
  const copied = nodeDiagnosticText(target.name, diagnostics)
  assert.ok(copied.includes(gatewayId))
  assert.equal(diagnostics.hops[2]?.via, "去程传输方式未知")
  assert.match(copied, /回程 工作电脑 → https:\/\/return\.example → 远程接入机/)
})

test("LAN does not identify the native client's ephemeral QUIC ID as a forwarding node", () => {
  const diagnostics = inspect({
    connection: { kind: "lan", device_id: entryId, addrs: ["192.0.2.1:3340"] },
    focus: { deviceId: entryId, name: info.name, entry: true },
    response: { nodes: [], session_path: { kind: "quic", checked_at_ms: checkedAt, upstream_device_id: gatewayId, return_path: { kind: "direct" } } },
  })
  assert.equal(diagnostics.pathLabel, "局域网直连")
  assert.equal(diagnostics.version, info.version)
  assert.deepEqual(diagnostics.hops.map(hop => hop.kind), ["client", "node"])
  assert.doesNotMatch(nodeDiagnosticText(info.name, diagnostics), new RegExp(gatewayId))
})

test("an unknown peer path does not come from relay capability or a URL with credentials", () => {
  const relayAddress = new URL("https://relay.example")
  relayAddress.username = "fixture"
  relayAddress.password = "example"
  const diagnostics = inspect({ response: {
    session_path: { kind: "websocket", checked_at_ms: checkedAt },
    nodes: [{ ...peer, observation: { checked_at_ms: checkedAt, last_synced_at_ms: null, path: { kind: "relay", url: relayAddress.href } } }],
  } })
  assert.equal(diagnostics.relay, true)
  assert.equal(diagnostics.status, "online")
  assert.equal(diagnostics.pathLabel, "路径未知")
  assert.deepEqual(diagnostics.hops.map(hop => hop.kind), ["client", "node", "node"])
  assert.equal(diagnostics.hops.at(-1)?.via, "路径未知")
})

test("offline and expired status never pass for a current path", () => {
  const response = {
    nodes: [{ ...peer, observation: { checked_at_ms: checkedAt, last_synced_at_ms: lastSyncedAt, path: { kind: "direct" } } }],
    session_path: { kind: "websocket", checked_at_ms: checkedAt },
  }
  const stale = inspect({ response, fresh: false })
  assert.equal(stale.status, "stale")
  assert.match(stale.pathLabel ?? "", /^上次/)
  const unreachable = inspect({ response: { ...response, nodes: [{ ...response.nodes[0], online: false }] } })
  assert.equal(unreachable.status, "unreachable")
  assert.equal(unreachable.hops.at(-1)?.via, "未收到响应")
  assert.equal(unreachable.lastSyncedAt, lastSyncedAt)
  const skipped = inspect({ response: {
    ...response,
    nodes: [{ ...peer, online: false, observation: { checked_at_ms: null, last_synced_at_ms: lastSyncedAt, path: null } }],
  } })
  assert.equal(skipped.status, "syncFailed")
  assert.equal(skipped.checkedAt, null)
  assert.equal(skipped.lastSyncedAt, lastSyncedAt)
  assert.equal(skipped.hops.at(-1)?.via, "上次同步失败")
  const disconnected = inspect({ response, connected: false })
  assert.equal(disconnected.status, "unknown")
  assert.equal(disconnected.capabilitiesStale, true)
  assert.deepEqual(disconnected.hops, [])
})
