import assert from "node:assert/strict"
import { readdirSync, readFileSync, statSync } from "node:fs"
import { join } from "node:path"
import test from "node:test"
import { fileURLToPath } from "node:url"
import ts from "typescript"

// Copy rules from docs/design/web-ux.md: jargon may only appear inside a folded <Details> or <SettingsDisclosure>.
const folded = ["Details", "SettingsDisclosure"]
const banned = [
  "节点", "Node", "Gateway", "网关", "入口", "Relay", "中继", "同步角色", "调度", "Peer ID", "设备编号", "设备密钥", "公钥", "私钥", "Noise", "握手",
  "capability", "凭证", "签名", "成员目录", "授权记录", "个人网络", "恢复包", "口令", "管理 Agent", "协调 Agent", "原生工作", "原生会话",
  "Agent 任务", "Provider", "SDK", "sidecar", "待核对", "核对", "复查", "本人", "证据", "轮次", "投递", "localhost", "JSON", "十六进制", "epoch", "角色",
  "密文", "链上", "收据", "意图", "对账", "证明", "落盘", "债权", "SHA-256", "Groth16", "窗口",
]
const allowed = ["安全码", "核对码", "Node.js"]
const cjk = /[一-鿿]/

export function violations(source: string, file = "x.tsx") {
  const found: string[] = []
  const sf = ts.createSourceFile(file, source, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX)
  const inDetails = (node: ts.Node): boolean => {
    for (let p = node.parent; p; p = p.parent) {
      if (ts.isJsxElement(p) && folded.includes(p.openingElement.tagName.getText(sf))) return true
    }
    return false
  }
  const check = (node: ts.Node, text: string) => {
    if (!cjk.test(text) && !ts.isJsxText(node)) return
    let clean = text
    for (const word of allowed) clean = clean.split(word).join("")
    const hit = banned.find((word) => clean.includes(word))
    if (hit && !inDetails(node)) found.push(`${file}:${sf.getLineAndCharacterOfPosition(node.getStart(sf)).line + 1} 「${hit}」 ${text.trim().slice(0, 40)}`)
  }
  const visit = (node: ts.Node) => {
    if (ts.isJsxText(node)) check(node, node.text)
    else if (ts.isStringLiteral(node) || ts.isNoSubstitutionTemplateLiteral(node)) check(node, node.text)
    else if (ts.isTemplateExpression(node)) check(node, [node.head.text, ...node.templateSpans.map((span) => span.literal.text)].join(""))
    ts.forEachChild(node, visit)
  }
  visit(sf)
  return found
}

function sources(dir: string): string[] {
  return readdirSync(dir).flatMap((name) => {
    const path = join(dir, name)
    if (statSync(path).isDirectory()) return sources(path)
    return /\.tsx?$/.test(name) && !name.endsWith(".test.ts") && !name.endsWith(".gen.ts") ? [path] : []
  })
}

test("copy check flags jargon outside Details only", () => {
  assert.equal(violations(`const a = <p>连接节点</p>`).length, 1)
  assert.equal(violations(`const a = <Details><p>节点</p></Details>`).length, 0)
  assert.equal(violations(`const a = <p>安全码一致</p>`).length, 0)
  assert.equal(violations(`throw new Error("请填写本人证据")`).length, 1)
  assert.equal(violations(`rpc("node_settings")`).length, 0)
})

test("user-facing copy avoids jargon outside Details", () => {
  // The shared client package and the phone app carry the same copy, so they are checked here too.
  const roots = ["..", "../../../../../packages/client/src/", "../../../../../apps/mobile/app/", "../../../../../apps/mobile/src/"]
  const found = roots.map((path) => new URL(path, import.meta.url)).flatMap((url) => {
    const root = fileURLToPath(url)
    return sources(root).flatMap((file) => violations(readFileSync(file, "utf8"), file.slice(root.length)))
  })
  assert.deepEqual(found, [])
})
