import { CameraView, useCameraPermissions } from 'expo-camera'
import { Camera } from 'lucide-react-native'
import { useCallback, useEffect, useState } from 'react'
import { AppState, Linking, View } from 'react-native'
import { ErrorText, useAction } from '@/components/action'
import { Panel } from '@/components/kit'
import { Button } from '@/components/arc/button'
import { Text } from '@/components/arc/text'

/** The camera reading one QR code; `title` names what is scanned, and pasting the link is always the way out. */
export function Scanner({ title, onScanned, onCancel }: { title: string; onScanned: (link: string) => void; onCancel: () => void }) {
  const [permission, requestPermission, getPermission] = useCameraPermissions({ get: false })
  const [readFailed, setReadFailed] = useState(false)
  const [cameraFailed, setCameraFailed] = useState(false)
  const { busy, error, run } = useAction()
  const checkPermission = useCallback(() => {
    void getPermission().then(() => setReadFailed(false), () => setReadFailed(true))
  }, [getPermission])
  useEffect(() => {
    checkPermission()
    const subscription = AppState.addEventListener('change', (state) => {
      if (state === 'active') checkPermission()
    })
    return () => subscription.remove()
  }, [checkPermission])

  const blocked = permission?.canAskAgain === false
  const allowCamera = () => void run('permission', async () => {
    if (blocked) {
      await Linking.openSettings().catch(() => { throw new Error('打不开系统设置。请手动打开设置，找到 Vibeface 并允许使用相机。') })
    } else {
      await requestPermission().catch(() => { throw new Error('无法请求相机权限。请重试。') })
    }
  })

  return <Panel icon={Camera} title={title}>
    {readFailed ? <>
      <ErrorText error="无法读取相机权限。" />
      <Button className="self-start" onPress={checkPermission}><Text>重试</Text></Button>
    </> : !permission ? <Text role="status" accessibilityLiveRegion="polite" className="text-muted">正在读取相机权限…</Text>
      : !permission.granted ? <>
        <Text className="text-muted">{blocked ? '请在系统设置中允许 Vibeface 使用相机。' : '扫码需要相机权限。'}</Text>
        <ErrorText error={error} />
        <Button className="self-start" disabled={busy !== null} onPress={allowCamera}><Text>{blocked ? '打开系统设置' : '允许使用相机'}</Text></Button>
      </> : cameraFailed ? <>
        <ErrorText error="无法启动相机。" />
        <Button className="self-start" onPress={() => setCameraFailed(false)}><Text>重试</Text></Button>
      </> : <View className="aspect-square overflow-hidden rounded-panel bg-surface-muted">
        <CameraView style={{ flex: 1 }} barcodeScannerSettings={{ barcodeTypes: ['qr'] }} onMountError={() => setCameraFailed(true)} onBarcodeScanned={({ data }) => onScanned(data)} />
      </View>}
    <Button variant="ghost" className="-ms-3 self-start" onPress={onCancel}><Text>不扫了，粘贴链接</Text></Button>
  </Panel>
}
