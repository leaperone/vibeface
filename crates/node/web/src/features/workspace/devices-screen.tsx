"use client"

import { useState } from "react"
import { Checkbox } from "@/components/ui/checkbox"
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card"
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible"
import { Field, FieldGroup, FieldLabel, FieldLegend, FieldSet } from "@/components/ui/field"
import { useWorkspace } from "./context"
import { nodeName, roleNames } from "./labels"
import { ActionButton, DetailList, EmptyState, StatusBadge, SubmitButton } from "./present"

const roles = [
  ["replica", "同步", true],
  ["scheduler", "调度", true],
  ["executor", "执行", true],
  ["relay", "中继", false],
  ["gateway", "接入", false],
] as const

export function DevicesScreen() {
  const workspace = useWorkspace()
  const [selected, setSelected] = useState<string[]>(roles.filter((role) => role[2]).map((role) => role[0]))
  const [busy, setBusy] = useState(false)
  const [invite, setInvite] = useState("")

  return (
    <section aria-label="我的设备" hidden={workspace.view !== "devices"}>
      <div>
        <h2>连接你的设备</h2>
        <p>每个地址只是入口。设备使用固定身份同步会话和任务。</p>
      </div>
      {workspace.nodes.length ? workspace.nodes.map((node) => (
        <Card key={node.node_id}>
          <CardHeader>
            <CardTitle>{nodeName(node.node_id)}{node.local ? " · 当前设备" : ""}</CardTitle>
            <StatusBadge value={node.online ? "ready" : undefined} label={node.online ? undefined : "离线"} />
          </CardHeader>
          <CardContent>
            <code>{node.node_id}</code>
            <DetailList pairs={[
              ["角色", (node.roles ?? []).map((role) => roleNames[role] || role).join("、")],
              ["执行器", (node.capabilities?.executors ?? []).join("、") || "未配置"],
              ["调度 Agent", node.capabilities?.manager ? "已配置" : "未配置"],
              ["允许目录", (node.capabilities?.roots ?? []).join("\n") || "未提供"],
            ]} />
            {node.local ? null : (
              <Collapsible>
                <CollapsibleTrigger>撤销此设备</CollapsibleTrigger>
                <CollapsibleContent>
                  <p>撤销会阻止设备发起新操作。离线设备要收到撤销信息后才会生效，正在运行的进程不会因此自动停止。</p>
                  <ActionButton variant="destructive" action={() => workspace.run(() => workspace.revoke(node.node_id))}>确认撤销设备</ActionButton>
                </CollapsibleContent>
              </Collapsible>
            )}
          </CardContent>
        </Card>
      )) : <EmptyState title="还没有设备信息" detail="刷新以读取此入口已知的个人网络。" />}
      <section>
        <h3>配对另一台设备</h3>
        <form
          onSubmit={(event) => {
            event.preventDefault()
            setBusy(true)
            void workspace.run(async () => {
              setInvite(await workspace.invite(selected))
            }).finally(() => setBusy(false))
          }}
        >
          <FieldGroup>
            <FieldSet>
              <FieldLegend>允许新设备承担的角色</FieldLegend>
              {roles.map(([value, label]) => (
                <Field key={value}>
                  <div className="flex items-center gap-2">
                    <Checkbox
                      id={`role-${value}`}
                      checked={selected.includes(value)}
                      onCheckedChange={(checked: boolean | "indeterminate") => {
                        setSelected((current) => checked === true ? [...new Set([...current, value])] : current.filter((role) => role !== value))
                      }}
                    />
                    <FieldLabel htmlFor={`role-${value}`}>{label}</FieldLabel>
                  </div>
                </Field>
              ))}
            </FieldSet>
            <SubmitButton busy={busy} variant="secondary">生成配对码</SubmitButton>
          </FieldGroup>
        </form>
        {invite ? (
          <div>
            <p>在你要加入的设备上使用此配对信息。配对码可授予访问权限，请只交给你自己的设备。</p>
            <pre>{invite}</pre>
          </div>
        ) : null}
      </section>
    </section>
  )
}
