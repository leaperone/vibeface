"use client"

import { InlineError } from "@/components/arc/product-kit"
import { useWorkspace } from "@vibeface/client/workspace/context.tsx"
import { Button } from "@vibeface/uiarc/button/button.tsx"
import { useState,type ReactNode } from "react"

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
  variant?: "primary" | "secondary" | "ghost" | "danger"
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
      size="md"
      title={title ?? label}
      aria-label={label}
      disabled={disabled || busy}
      loading={busy}
      onClick={() => {
        if (busy) return
        const generation = workspace.generation
        setBusyGeneration(generation)
        setFailure(null)
        void action().catch(setFailure).finally(() => setBusyGeneration(owner => owner === generation ? null : owner))
      }}
    >
      {children}
    </Button>
    <InlineError error={failure} />
    </span>
  )
}
