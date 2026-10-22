"use client"

import { useState, type ReactNode } from "react"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Spinner } from "@/components/ui/spinner"
import { InlineError } from "@/components/app/kit"
import { stateLabel } from "@vibeface/client/workspace/labels.ts"
import { useWorkspace } from "@vibeface/client/workspace/context.tsx"

export function StatusBadge({ value, label }: { value?: string; label?: string }) {
  const text = stateLabel(value, label)
  return <Badge variant={value === "failed" || value === "rejected" ? "destructive" : "secondary"}>{text}</Badge>
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

export function ActionButton({
  children,
  className,
  title,
  label,
  variant = "secondary",
  disabled,
  action,
}: {
  children: ReactNode
  className?: string
  title?: string
  /** Makes it an icon button: `children` is the icon, `label` its accessible name and tooltip. */
  label?: string
  variant?: "default" | "secondary" | "ghost" | "destructive"
  disabled?: boolean
  action: () => Promise<void>
}) {
  const workspace = useWorkspace()
  const [busyGeneration, setBusyGeneration] = useState<number | null>(null)
  const busy = busyGeneration === workspace.generation
  const [failure, setFailure] = useState<unknown>(null)
  return (
    <span className="inline-grid gap-1">
    <Button
      type="button"
      variant={variant}
      className={className}
      size={label ? "icon" : undefined}
      title={title ?? label}
      aria-label={label}
      disabled={disabled || busy}
      aria-busy={busy}
      onClick={() => {
        if (busy) return
        const generation = workspace.generation
        setBusyGeneration(generation)
        setFailure(null)
        void action().catch(setFailure).finally(() => setBusyGeneration(owner => owner === generation ? null : owner))
      }}
    >
      {busy ? <Spinner /> : null}
      {busy && label ? null : children}
    </Button>
    <InlineError error={failure} />
    </span>
  )
}

export function SubmitButton({ children, busy, disabled, variant = "default" }: { children: ReactNode; busy: boolean; disabled?: boolean; variant?: "default" | "secondary" }) {
  return (
    <Button type="submit" variant={variant} disabled={disabled || busy} aria-busy={busy}>
      {busy ? <Spinner /> : null}
      {children}
    </Button>
  )
}
