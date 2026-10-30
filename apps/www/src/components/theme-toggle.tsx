import { useTheme } from 'fumadocs-ui/provider/base'
import { useEffect, useState } from 'react'

export function ThemeToggle() {
  const { resolvedTheme, setTheme } = useTheme()
  const [mounted, setMounted] = useState(false)
  useEffect(() => setMounted(true), [])
  const dark = mounted && resolvedTheme === 'dark'
  return <button className="theme-toggle" type="button" aria-label={dark ? '切换到浅色主题' : '切换到深色主题'} onClick={() => setTheme(dark ? 'light' : 'dark')}>{dark ? '浅色' : '深色'}</button>
}
