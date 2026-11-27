import { useElapsed } from "@vibeface/client/workspace/elapsed.ts"

/** A task's run time, counting while `active`; `dot` leads it with the separator of the line it joins. */
export function Elapsed({ start, end, active, dot = false }: { start?: number | null; end?: number | null; active: boolean; dot?: boolean }) {
  const label = useElapsed(start, end, active)
  if (!label) return null
  return <>{dot ? <span aria-hidden="true">·</span> : null}<span className="tabular-nums"><span className="sr-only">用时 </span>{label}</span></>
}
