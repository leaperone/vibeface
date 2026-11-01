"use client"

import { useState, type ReactNode } from "react"
import { Button } from "@/components/ui/button"
import { Spinner } from "@/components/ui/spinner"
import { InlineError } from "@/components/app/kit"
import { useWorkspace } from "@vibeface/client/workspace/context.tsx"

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
