import type { ReactNode } from "react"
import { WrenchIcon } from "lucide-react"

import {
  TraceCollapsible,
  TraceCollapsibleContent,
  TraceCollapsibleTrigger,
  TraceIcon,
  TraceLine,
  TraceTitle,
} from "@/components/ui/trace"

export function Tool({
  title,
  status,
  children,
}: {
  title: string
  status?: "running" | "error" | "pending"
  children?: ReactNode
}) {
  return (
    <TraceCollapsible defaultOpen={false}>
      <TraceCollapsibleTrigger>
        <TraceLine status={status}>
          <TraceIcon>
            <WrenchIcon />
          </TraceIcon>
          <TraceTitle>{title}</TraceTitle>
        </TraceLine>
      </TraceCollapsibleTrigger>
      {children ? <TraceCollapsibleContent>{children}</TraceCollapsibleContent> : null}
    </TraceCollapsible>
  )
}
