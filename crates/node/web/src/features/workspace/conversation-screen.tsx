"use client"

import { type ChangeEvent } from "react"
import { ConversationScreen } from "@/features/chat/conversation-screen"
import { Field, FieldLabel } from "@/components/ui/field"
import { NativeSelect, NativeSelectOption } from "@/components/ui/native-select"
import { nodeName } from "./labels"
import { useWorkspace } from "./context"

export function ConversationView() {
  const workspace = useWorkspace()
  const schedulers = workspace.nodes.filter((node) => node.roles?.includes("scheduler") && node.capabilities?.manager)
  return (
    <div hidden={workspace.view !== "chat"}>
      <Field>
        <FieldLabel htmlFor="scheduler-node">调度设备</FieldLabel>
        <NativeSelect
          id="scheduler-node"
          name="scheduler"
          value={workspace.schedulerNodeId}
          onChange={(event: ChangeEvent<HTMLSelectElement>) => workspace.setSchedulerNodeId(event.target.value)}
        >
          {schedulers.length ? schedulers.map((node) => (
            <NativeSelectOption key={node.node_id} value={node.node_id}>
              {nodeName(node.node_id)}{node.local ? " · 本机" : ""}{node.online ? "" : " · 离线"}
            </NativeSelectOption>
          )) : <NativeSelectOption value="">没有可用的调度 Agent</NativeSelectOption>}
        </NativeSelect>
      </Field>
      <ConversationScreen />
    </div>
  )
}
