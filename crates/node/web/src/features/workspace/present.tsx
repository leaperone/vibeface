"use client"

import { useState, type ReactNode } from "react"
import { ChevronDownIcon } from "lucide-react"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible"
import { Empty, EmptyDescription, EmptyHeader, EmptyTitle } from "@/components/ui/empty"
import { ScrollArea } from "@/components/ui/scroll-area"
import { Spinner } from "@/components/ui/spinner"
import { stateLabel, textOf } from "./labels"

export function StatusBadge({ value, label }: { value?: string; label?: string }) {
  const text = stateLabel(value, label)
  return <Badge variant={value === "failed" || value === "rejected" ? "destructive" : "secondary"}>{text}</Badge>
}

export function EmptyState({ title, detail }: { title: string; detail: string }) {
  return (
    <Empty>
      <EmptyHeader>
        <EmptyTitle>{title}</EmptyTitle>
        <EmptyDescription>{detail}</EmptyDescription>
      </EmptyHeader>
    </Empty>
  )
}

export function DetailList({ pairs }: { pairs: Array<[string, string | undefined]> }) {
  return (
    <dl className="grid gap-3">
      {pairs.map(([key, value]) => (
        <div key={key}>
          <dt>{key}</dt>
          <dd className="whitespace-pre-line">{value ?? "未提供"}</dd>
        </div>
      ))}
    </dl>
  )
}

export function RawDetails({ title, value, defaultOpen = false }: { title: string; value: unknown; defaultOpen?: boolean }) {
  return (
    <Collapsible defaultOpen={defaultOpen}>
      <CollapsibleTrigger asChild>
        <Button variant="ghost" size="sm" className="group -ms-2 h-auto min-h-8 whitespace-normal text-start">
          {title}
          <ChevronDownIcon aria-hidden="true" className="shrink-0 group-data-[state=open]:rotate-180" />
        </Button>
      </CollapsibleTrigger>
      <CollapsibleContent className="pt-3">
        <ScrollArea>
          <pre>{textOf(value)}</pre>
        </ScrollArea>
      </CollapsibleContent>
    </Collapsible>
  )
}

export function ActionButton({
  children,
  className,
  variant = "secondary",
  disabled,
  action,
}: {
  children: ReactNode
  className?: string
  variant?: "default" | "secondary" | "ghost" | "destructive"
  disabled?: boolean
  action: () => Promise<void>
}) {
  const [busy, setBusy] = useState(false)
  return (
    <Button
      type="button"
      variant={variant}
      className={className}
      disabled={disabled || busy}
      aria-busy={busy}
      onClick={() => {
        if (busy) return
        setBusy(true)
        void action().finally(() => setBusy(false))
      }}
    >
      {busy ? <Spinner /> : null}
      {children}
    </Button>
  )
}

export function SubmitButton({ children, busy, disabled, variant = "default" }: { children: ReactNode; busy: boolean; disabled?: boolean; variant?: "default" | "secondary" }) {
  return (
    <Button type="submit" variant={variant} disabled={busy || disabled} aria-busy={busy}>
      {busy ? <Spinner /> : null}
      {children}
    </Button>
  )
}
