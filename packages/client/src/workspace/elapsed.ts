import { useEffect, useState } from 'react'

/** "45 秒", "3 分 12 秒", "2 小时 5 分". Clocks of different devices may skew, so a negative span reads as 0. */
export function elapsedLabel(ms: number): string {
  const seconds = Math.max(0, Math.floor(ms / 1000))
  if (seconds < 60) return `${seconds} 秒`
  const minutes = Math.floor(seconds / 60)
  if (minutes < 60) return `${minutes} 分 ${seconds % 60} 秒`
  return `${Math.floor(minutes / 60)} 小时 ${minutes % 60} 分`
}

/**
 * How long a task has run, from its dispatch to its result, or to now while `active`; null without a dispatch time,
 * or when it ended without a result time.
 */
export function useElapsed(start: number | null | undefined, end: number | null | undefined, active: boolean): string | null {
  const running = start != null && end == null && active
  const [now, setNow] = useState(Date.now)
  useEffect(() => {
    if (!running) return
    setNow(Date.now())
    const timer = setInterval(() => setNow(Date.now()), 1000)
    return () => clearInterval(timer)
  }, [running])
  if (start == null) return null
  if (end != null) return elapsedLabel(end - start)
  return running ? elapsedLabel(now - start) : null
}
