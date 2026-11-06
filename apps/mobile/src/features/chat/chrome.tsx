import { GlassView, isGlassEffectAPIAvailable, isLiquidGlassAvailable } from 'expo-glass-effect'
import { useEffect, useState } from 'react'
import { AccessibilityInfo, Platform, StyleSheet, View, type ViewProps } from 'react-native'
import { useUniwind } from 'uniwind'

export type ChatChrome = { kind: 'glass'; colorScheme: 'light' | 'dark' } | { kind: 'solid' }

export function useChatChrome(): ChatChrome {
  const { theme } = useUniwind()
  const available = Platform.OS === 'ios' && isGlassEffectAPIAvailable() && isLiquidGlassAvailable()
  const [reduced, setReduced] = useState(true)
  useEffect(() => {
    if (!available) return
    let current = true, changed = false
    const subscription = AccessibilityInfo.addEventListener('reduceTransparencyChanged', (enabled) => { changed = true; setReduced(enabled) })
    void AccessibilityInfo.isReduceTransparencyEnabled().then((enabled) => {
      // A settings event after subscribing is newer than the initial asynchronous read.
      if (current && !changed) setReduced(enabled)
    }).catch(() => {})
    return () => { current = false; subscription.remove() }
  }, [available])
  return available && !reduced ? { kind: 'glass', colorScheme: theme === 'dark' ? 'dark' : 'light' } : { kind: 'solid' }
}

export function ChatMaterial({ chrome, radius = 26, children, ...props }: ViewProps & { chrome: ChatChrome; radius?: number }) {
  return <View {...props}>
    {chrome.kind === 'glass'
      ? <GlassView pointerEvents="none" glassEffectStyle="regular" colorScheme={chrome.colorScheme} style={[StyleSheet.absoluteFill, { borderRadius: radius }]} />
      : <View pointerEvents="none" className="border border-input bg-card" style={[StyleSheet.absoluteFill, { borderRadius: radius }]} />}
    {children}
  </View>
}
