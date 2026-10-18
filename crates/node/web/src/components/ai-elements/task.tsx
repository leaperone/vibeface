import type { ReactNode } from "react"
import { ListChecksIcon } from "lucide-react"

import {
  TraceCollapsible,
  TraceCollapsibleContent,
  TraceCollapsibleTrigger,
  TraceIcon,
  TraceLine,
  TraceTitle,
} from "@/components/ui/trace"

export function Task({
  title,
  status,
  mark,
  children,
}: {
  title: string
  status?: "running" | "error" | "pending"
  mark?: ReactNode
  children?: ReactNode
}) {
  return (
    <TraceCollapsible defaultOpen={false}>
      <TraceCollapsibleTrigger>
        <TraceLine status={status}>
          <TraceIcon>
            <ListChecksIcon />
          </TraceIcon>
          {mark}
          <TraceTitle>{title}</TraceTitle>
        </TraceLine>
      </TraceCollapsibleTrigger>
      {children ? <TraceCollapsibleContent>{children}</TraceCollapsibleContent> : null}
    </TraceCollapsible>
  )
}
