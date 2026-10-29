import type { ReactNode } from "react"
import { Eye, Palette, RotateCcw, Shapes, type LucideIcon } from "lucide-react"
import { colors, EYES, HUES, OUTLINES, type Avatar, type Face } from "@vibeface/client/avatar/face.ts"
import { eyeNames, hueNames, outlineNames, withPart } from "@vibeface/client/avatar/pick.ts"
import { BotFace, useDark } from "@/components/app/bot-face"
import { cn } from "@/lib/utils"

/** Outline, eyes and colour as rows of pressable previews, and a way back to the face the id gives. Each press applies at once. */
export function AvatarPicker({ botId, avatar, face, disabled, pick }: { botId: string; avatar: Avatar | null; face: Face; disabled: boolean; pick: (avatar: Avatar | null) => void }) {
  const dark = useDark()
  return <div className="grid gap-1 px-2 py-2">
    <Choices icon={Shapes} label="轮廓">
      {OUTLINES.map((outline) => <Choice key={outline} label={outlineNames[outline]} on={face.outline === outline} disabled={disabled} onPress={() => pick(withPart(face, { outline }))}>
        <BotFace bot={{ bot_id: botId, avatar: withPart(face, { outline }) }} mood="idle" size={28} />
      </Choice>)}
    </Choices>
    <Choices icon={Eye} label="眼睛">
      {EYES.map((eyes) => <Choice key={eyes} label={eyeNames[eyes]} on={face.eyes === eyes} disabled={disabled} onPress={() => pick(withPart(face, { eyes }))}>
        <BotFace bot={{ bot_id: botId, avatar: withPart(face, { eyes }) }} mood="idle" size={28} />
      </Choice>)}
    </Choices>
    <Choices icon={Palette} label="颜色">
      {HUES.map((hue) => <Choice key={hue} label={hueNames[hue]} on={face.hue === hue} disabled={disabled} onPress={() => pick(withPart(face, { hue }))}>
        <span aria-hidden="true" className="size-6 rounded-full" style={{ background: colors({ ...face, hue }, dark, 1).base }} />
      </Choice>)}
    </Choices>
    <button type="button" disabled={disabled || !avatar} onClick={() => pick(null)}
      className="flex min-h-11 items-center gap-2 self-start rounded-lg px-2 text-meta text-muted-foreground outline-none transition-colors focus-visible:ring-3 focus-visible:ring-ring/50 enabled:hover:bg-muted enabled:hover:text-foreground disabled:opacity-50">
      <RotateCcw aria-hidden="true" className="size-4" />恢复默认
    </button>
  </div>
}

function Choices({ icon: Icon, label, children }: { icon: LucideIcon; label: string; children: ReactNode }) {
  return <div role="group" aria-label={label} className="flex items-center gap-1">
    <Icon aria-hidden="true" className="mx-2 size-4 shrink-0 text-muted-foreground" />
    <div className="flex flex-wrap gap-0.5">{children}</div>
  </div>
}

function Choice({ label, on, disabled, onPress, children }: { label: string; on: boolean; disabled: boolean; onPress: () => void; children: ReactNode }) {
  return <button type="button" aria-label={label} title={label} aria-pressed={on} disabled={disabled} onClick={on ? undefined : onPress}
    className={cn("grid size-11 place-items-center rounded-lg outline-none transition-colors focus-visible:ring-3 focus-visible:ring-ring/50 enabled:hover:bg-muted disabled:cursor-not-allowed",
      on ? "bg-muted ring-2 ring-brand ring-inset" : "disabled:opacity-50")}>
    {children}
  </button>
}
