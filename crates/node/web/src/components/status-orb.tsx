import { ThinkingOrb, type OrbSize } from "thinking-orbs"
import { orbStateFor, type OrbPlace } from "@/features/chat/orb-state"

const labels: Record<OrbPlace, string> = {
  connection: "正在连接",
  wait: "稍等",
  refresh: "正在查找",
  reconcile: "正在确认",
  "idle-chat": "等你发消息",
  "idle-link": "已连接",
  trace: "多步进行中",
  reply: "正在回复",
  plan: "正在整理计划",
}

export function StatusOrb({
  place,
  size = 20,
  label,
  decorative = true,
}: {
  place: OrbPlace
  size?: OrbSize
  label?: string
  decorative?: boolean
}) {
  return (
    <span className="inline-flex shrink-0 items-center justify-center" style={{ width: size, height: size, maxWidth: 64, maxHeight: 64 }}>
      <ThinkingOrb
        state={orbStateFor(place)}
        size={size}
        theme="auto"
        aria-label={label ?? labels[place]}
        aria-hidden={decorative || undefined}
      />
    </span>
  )
}
