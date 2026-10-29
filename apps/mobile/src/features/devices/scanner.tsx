import { CameraView, useCameraPermissions } from 'expo-camera'
import { Camera } from 'lucide-react-native'
import { View } from 'react-native'
import { Panel } from '@/components/kit'
import { Button } from '@/components/ui/button'
import { Text } from '@/components/ui/text'

/** The camera reading one QR code; `title` names what is scanned, and pasting the link is always the way out. */
export function Scanner({ title, onScanned, onCancel }: { title: string; onScanned: (link: string) => void; onCancel: () => void }) {
  const [permission, request] = useCameraPermissions()
  if (!permission) return null
  if (!permission.granted) return <Panel icon={Camera} title={title}>
    <Text className="text-muted-foreground">需要相机权限才能扫码，也可以粘贴链接</Text>
    <View className="flex-row gap-2">
      <Button variant="ghost" onPress={onCancel}><Text>不扫了，粘贴链接</Text></Button>
      <Button onPress={request}><Text>允许使用相机</Text></Button>
    </View>
  </Panel>
  return <Panel icon={Camera} title={title}>
    <View className="aspect-square overflow-hidden rounded-xl bg-muted">
      <CameraView style={{ flex: 1 }} barcodeScannerSettings={{ barcodeTypes: ['qr'] }} onBarcodeScanned={({ data }) => onScanned(data)} />
    </View>
    <Button variant="ghost" className="-ms-3 self-start" onPress={onCancel}><Text>不扫了，粘贴链接</Text></Button>
  </Panel>
}
