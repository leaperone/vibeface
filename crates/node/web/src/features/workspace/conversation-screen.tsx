"use client"

import { type ChangeEvent, type ReactNode } from "react"
import { Field, FieldLabel } from "@/components/ui/field"
import { NativeSelect, NativeSelectOption } from "@/components/ui/native-select"
import { nodeName } from "./labels"
import { useWorkspace } from "./context"
import type { ChatSlotState } from "./types"

export function ChatSlot({ children }: { children?: ReactNode }) {
  return (
    <section aria-label="调度会话" data-slot="chat">
      {children}
    </section>
  )
}

export function ConversationView({ chat }: { chat?: ReactNode | ((slot: ChatSlotState) => ReactNode) }) {
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
      <ChatSlot>{typeof chat === "function" ? chat(workspace.chatSlot) : chat}</ChatSlot>
    </div>
  )
}
