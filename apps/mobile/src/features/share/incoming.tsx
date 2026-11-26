import { useEffect, useRef } from 'react'
import { Alert, AppState } from 'react-native'
import { addEventListener } from 'expo-linking'
import { clearSharedPayloads, getSharedPayloads } from 'expo-sharing'
import { usePathname, useRootNavigationState, useRouter } from 'expo-router'
import { messageOf } from '@vibeface/client/lib/node.ts'
import { useWorkspace } from '@/data/context'
import { mobilePlatform } from '@/platform'
import { incomingShares, shareVisible } from '@/platform/incoming-share'

export const shareInbox = incomingShares(mobilePlatform.storage)

/** Native cold starts, foreground shares and resumes hand off to a separate durable draft. */
export function IncomingShareEntry() {
  const router = useRouter()
  const workspace = useWorkspace()
  const navigation = useRootNavigationState()
  const pathname = usePathname()
  const path = useRef(pathname)
  path.current = pathname
  const opened = useRef<string | null>(null)
  useEffect(() => {
    if (!navigation?.key) return
    const open = () => {
      const first = shareInbox.read().find(item => shareVisible(item, workspace))
      if (first && opened.current !== first.material.id) {
        opened.current = first.material.id
        if (path.current !== '/share') router.push('/share')
      }
    }
    const receive = () => {
      let raw: string | undefined
      try {
        const payloads = getSharedPayloads()
        raw = JSON.stringify(payloads)
        shareInbox.receive(payloads)
        if (payloads.length) {
          clearSharedPayloads()
          shareInbox.receive([])
        }
        open()
      } catch (cause) {
        Alert.alert('资料尚未保存', messageOf(cause), [
          { text: '保留，稍后处理', style: 'cancel' },
          ...(raw && raw !== '[]' ? [{ text: '放弃这次分享', style: 'destructive' as const, onPress: () => {
            // Another app may have shared something else while the alert was open.
            if (JSON.stringify(getSharedPayloads()) === raw) clearSharedPayloads()
          } }] : []),
        ])
      }
    }
    const stop = shareInbox.watch(open)
    const foreground = AppState.addEventListener('change', state => { if (state === 'active') receive() })
    const linking = addEventListener('url', receive)
    receive()
    return () => { stop(); foreground.remove(); linking.remove() }
  }, [navigation?.key, router, workspace.networkId, workspace.deviceId])
  return null
}
