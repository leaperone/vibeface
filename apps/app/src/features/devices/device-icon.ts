import { terms } from "@/lib/glossary"
import type { Member } from "@vibeface/client/lib/node.ts"
import { deviceForm } from "@vibeface/client/workspace/labels.ts"
import { Smartphone, Tablet, type LucideIcon } from "lucide-react"

/** One icon per kind of device; which computer this browser goes through is a tag, not another icon. */
export function deviceIcon(row: Pick<Member, "kind" | "name">): LucideIcon {
  if (row.kind === "node") return terms.thisComputer.icon
  const form = deviceForm(row.kind, row.name)
  return form === "tablet" ? Tablet : form === "phone" ? Smartphone : terms.browser.icon
}
