"use client"

import { type ChangeEvent } from "react"
import { ConversationScreen } from "@/features/chat/conversation-screen"
import { Field, FieldLabel } from "@/components/ui/field"
import { NativeSelect, NativeSelectOption } from "@/components/ui/native-select"
import { nodeName } from "./labels"
import { useWorkspace } from "./context"
import { canSelectScheduler } from "./model"

export function ConversationView() {
  const workspace = useWorkspace()
  const schedulers = workspace.nodes.filter(canSelectScheduler)
  return (
    <div className="conversation-view" hidden={workspace.view !== "chat"}>
      <div className="shrink-0 px-4 py-2 sm:px-6">
        <Field orientation="horizontal" className="min-w-0 gap-2">
          <FieldLabel htmlFor="scheduler-node" className="flex-none text-xs text-muted-foreground">调度</FieldLabel>
          <NativeSelect
            className="min-w-0 max-w-full [&>select]:h-9 sm:[&>select]:h-8"
            id="scheduler-node"
            name="scheduler"
            value={workspace.schedulerNodeId}
            onChange={(event: ChangeEvent<HTMLSelectElement>) => workspace.setSchedulerNodeId(event.target.value)}
          >
            {schedulers.length ? schedulers.map((node) => (
              <NativeSelectOption key={node.node_id} value={node.node_id}>
                {nodeName(node.node_id)}{node.local ? " · 本机" : ""}{node.online ? node.capabilities ? "" : " · 能力待核对" : " · 离线，能力待核对"}
              </NativeSelectOption>
            )) : <NativeSelectOption value="">没有可用的调度 Agent</NativeSelectOption>}
          </NativeSelect>
        </Field>
      </div>
      <ConversationScreen />
    </div>
  )
}
