import * as Clipboard from 'expo-clipboard'
import { useLayoutEffect, useRef, useState } from 'react'
import { AccessibilityInfo } from 'react-native'

type CopyState = { kind: 'idle' } | { kind: 'pending'; key: string } | { kind: 'copied'; key: string } | { kind: 'failed' }

/** Feedback belongs to the displayed source and the latest copy, including its reset timer. */
export function useClipboardAction(source: unknown, duration = 1500) {
  const [state, setState] = useState<CopyState>({ kind: 'idle' })
  const scope = useRef(source)
  const mounted = useRef(false)
  const sequence = useRef(0)
  const timer = useRef<ReturnType<typeof setTimeout>>(undefined)
  function reset() {
    sequence.current++
    clearTimeout(timer.current)
    timer.current = undefined
    if (mounted.current) setState({ kind: 'idle' })
  }
  useLayoutEffect(() => {
    scope.current = source
    mounted.current = true
    reset()
    return () => {
      mounted.current = false
      reset()
    }
  }, [source])

  async function copy(value: string, key = 'value') {
    if (!mounted.current || scope.current !== source) return false
    reset()
    const owner = sequence.current
    const current = () => mounted.current && scope.current === source && sequence.current === owner
    setState({ kind: 'pending', key })
    try {
      await Clipboard.setStringAsync(value)
    } catch {
      if (current()) {
        setState({ kind: 'failed' })
        AccessibilityInfo.announceForAccessibility('复制失败，请重试')
      }
      return false
    }
    if (!current()) return false
    setState({ kind: 'copied', key })
    AccessibilityInfo.announceForAccessibility('已复制')
    timer.current = setTimeout(() => {
      if (current()) {
        setState({ kind: 'idle' })
        timer.current = undefined
      }
    }, duration)
    return true
  }

  const visible: CopyState = scope.current === source ? state : { kind: 'idle' }
  return {
    busy: visible.kind === 'pending',
    copied: visible.kind === 'copied' ? visible.key : null,
    error: visible.kind === 'failed' ? '复制失败，请重试' : '',
    copy, reset,
  }
}
