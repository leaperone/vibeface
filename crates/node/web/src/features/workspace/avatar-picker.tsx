import type { KeyboardEvent, ReactNode } from "react"
import { Eye, LoaderCircle, Palette, RotateCcw, Shapes, type LucideIcon } from "lucide-react"
import { colors, EYES, HUES, OUTLINES, type Avatar, type Face } from "@vibeface/client/avatar/face.ts"
import { eyeNames, hueNames, outlineNames, withPart } from "@vibeface/client/avatar/pick.ts"
import { BotFace, useDark } from "@/components/app/bot-face"
import { cn } from "@/lib/utils"

/**
 * Outline, eyes and colour as radio rows of still previews, and a way back to the face the id gives. Each press applies
 * at once; arrow keys move along a row and Space or Enter picks. While `busy` a pick is being written and the rows wait.
 */
export function AvatarPicker({ botId, avatar, face, disabled, busy, pick }: { botId: string; avatar: Avatar | null; face: Face; disabled: boolean; busy: boolean; pick: (avatar: Avatar | null) => void }) {
  const dark = useDark()
  const off = disabled || busy
  return <div aria-busy={busy} className="grid gap-1 px-2 py-2">
    <Choices icon={Shapes} label="轮廓">
      {OUTLINES.map((outline) => <Choice key={outline} label={outlineNames[outline]} on={face.outline === outline} disabled={off} onPress={() => pick(withPart(face, { outline }))}>
        <BotFace bot={{ bot_id: botId, avatar: withPart(face, { outline }) }} mood="idle" size={28} still />
      </Choice>)}
    </Choices>
    <Choices icon={Eye} label="眼睛">
      {EYES.map((eyes) => <Choice key={eyes} label={eyeNames[eyes]} on={face.eyes === eyes} disabled={off} onPress={() => pick(withPart(face, { eyes }))}>
        <BotFace bot={{ bot_id: botId, avatar: withPart(face, { eyes }) }} mood="idle" size={28} still />
      </Choice>)}
    </Choices>
    <Choices icon={Palette} label="颜色">
      {HUES.map((hue) => <Choice key={hue} label={hueNames[hue]} on={face.hue === hue} disabled={off} onPress={() => pick(withPart(face, { hue }))}>
        <span aria-hidden="true" className="size-6 rounded-full" style={{ background: colors({ ...face, hue }, dark, 1).base }} />
      </Choice>)}
    </Choices>
    <div className="flex min-h-11 items-center gap-2">
      <button type="button" disabled={off || !avatar} onClick={() => pick(null)}
        className="flex min-h-11 items-center gap-2 rounded-lg px-2 text-meta text-muted-foreground outline-none transition-colors focus-visible:ring-3 focus-visible:ring-ring/50 enabled:hover:bg-muted enabled:hover:text-foreground disabled:opacity-50">
        <RotateCcw aria-hidden="true" className="size-4" />恢复默认
      </button>
      <span role="status" className="flex items-center gap-1 text-meta text-muted-foreground">
        {busy ? <><LoaderCircle aria-hidden="true" className="size-4 animate-spin motion-reduce:animate-none" />正在换</> : null}
      </span>
    </div>
  </div>
}

/** Arrow keys move focus along the row, wrapping; only the picked choice is in the Tab order. */
function moveFocus(event: KeyboardEvent<HTMLDivElement>) {
  const step = { ArrowRight: 1, ArrowDown: 1, ArrowLeft: -1, ArrowUp: -1 }[event.key]
  if (!step) return
  const radios = [...event.currentTarget.querySelectorAll<HTMLButtonElement>("[role=radio]")]
  const at = radios.indexOf(document.activeElement as HTMLButtonElement)
  if (at < 0) return
  event.preventDefault()
  radios[(at + step + radios.length) % radios.length].focus()
}

function Choices({ icon: Icon, label, children }: { icon: LucideIcon; label: string; children: ReactNode }) {
  return <div className="flex items-center gap-1">
    <Icon aria-hidden="true" className="mx-2 size-4 shrink-0 text-muted-foreground" />
    <div role="radiogroup" aria-label={label} onKeyDown={moveFocus} className="flex flex-wrap gap-0.5">{children}</div>
  </div>
}

/** `aria-disabled` rather than `disabled`, so focus stays on the row while a pick is written. */
function Choice({ label, on, disabled, onPress, children }: { label: string; on: boolean; disabled: boolean; onPress: () => void; children: ReactNode }) {
  return <button type="button" role="radio" aria-checked={on} aria-label={label} title={label} aria-disabled={disabled || undefined} tabIndex={on ? 0 : -1}
    onClick={on || disabled ? undefined : onPress}
    className={cn("grid size-11 place-items-center rounded-lg outline-none transition-colors focus-visible:ring-3 focus-visible:ring-ring/50",
      disabled ? "cursor-not-allowed" : "hover:bg-muted", on ? "bg-muted ring-2 ring-brand ring-inset" : disabled && "opacity-50")}>
    {children}
  </button>
}
