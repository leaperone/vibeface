import * as DocumentPicker from 'expo-document-picker'
import { File } from 'expo-file-system'
import * as Haptics from 'expo-haptics'
import * as ImagePicker from 'expo-image-picker'
import { ArrowUp, LoaderCircle, Paperclip, Square, X } from 'lucide-react-native'
import { useEffect, useRef, type ReactNode } from 'react'
import { ActionSheetIOS, Alert, Platform, ScrollView, View } from 'react-native'
import { Button } from '@/components/arc/button'
import { Icon } from '@/components/arc/icon'
import { Text } from '@/components/arc/text'
import type { Attachment } from '@/data/types'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { privateFiles } from '@/platform/files'
import { ComposerInput, ComposerPanel } from '@/components/arc/chat'

type Props = {
  value: string
  onChange: (value: string) => void
  files: Attachment[]
  onFiles: (files: Attachment[]) => void
  attachmentScope: string
  /** Why sending is off, in place of the placeholder, or under the box while it holds a draft; null when it is allowed. */
  blocked: string | null
  sending: boolean
  onSend: () => void
  onStop?: () => void
  /** Status and actions shown below the input. */
  children?: ReactNode
  /** A small control beside the send button, inside the box. */
  target?: ReactNode
  sendLabel?: string
  label?: string
}

export function Composer({ value, onChange, files, onFiles, attachmentScope, blocked, sending, onSend, onStop, children, target, sendLabel = '发送', label = '给 Bot 发消息' }: Props) {
  const device = useDevice()
  const connectionGeneration = device.connectionGeneration()
  const ios = Platform.OS === 'ios'
  const enabled = !blocked && !sending
  const latest = useRef({ files, onFiles, attachmentScope, enabled, device, connectionGeneration, generation: 0, mounted: true })
  if (latest.current.attachmentScope !== attachmentScope || latest.current.enabled !== enabled || latest.current.connectionGeneration !== connectionGeneration) latest.current.generation++
  latest.current = { ...latest.current, files, onFiles, attachmentScope, enabled, device, connectionGeneration }
  useEffect(() => {
    latest.current.mounted = true
    return () => { latest.current.mounted = false; latest.current.generation++ }
  }, [])
  const picking = useRef(false)
  const drafting = value.trim().length > 0 || files.length > 0
  const canSend = !blocked && !sending && drafting
  const pick = async (source: 'photos' | 'files', generation: number, connection: number) => {
    if (picking.current || !latest.current.mounted || !latest.current.enabled || latest.current.generation !== generation || latest.current.device.connectionGeneration() !== connection) return
    picking.current = true
    const scope = latest.current.attachmentScope, request = ++latest.current.generation
    const current = () => latest.current.mounted && latest.current.enabled && latest.current.attachmentScope === scope && latest.current.generation === request && latest.current.device.connectionGeneration() === connection
    let fileCurrent = () => false
    try {
      await privateFiles.run(async (active) => {
        fileCurrent = active
        const sources: string[] = [], owned: File[] = [], added: Attachment[] = [], errors: unknown[] = []
        let handedOff = false
        try {
          if (source === 'photos') {
            const picked = await ImagePicker.launchImageLibraryAsync({ mediaTypes: ['images'], allowsMultipleSelection: true, quality: 1, preferredAssetRepresentationMode: ImagePicker.UIImagePickerPreferredAssetRepresentationMode.Current })
            if (picked.canceled) return
            sources.push(...picked.assets.map(asset => asset.uri))
            if (!current() || !fileCurrent()) return
            for (const asset of picked.assets) {
              const candidate = await privateFiles.retain(asset.uri)
              owned.push(candidate)
              if (!current() || !fileCurrent()) return
              const file = new File(asset.uri)
              added.push({ uri: candidate.uri, name: asset.fileName || file.name, size: asset.fileSize ?? file.size, mimeType: asset.mimeType || file.type || null })
            }
          } else {
            const picked = await DocumentPicker.getDocumentAsync({ multiple: true, copyToCacheDirectory: true })
            if (picked.canceled) return
            sources.push(...picked.assets.map(asset => asset.uri))
            if (!current() || !fileCurrent()) return
            for (const asset of picked.assets) {
              const candidate = await privateFiles.retain(asset.uri)
              owned.push(candidate)
              if (!current() || !fileCurrent()) return
              added.push({ uri: candidate.uri, name: asset.name, size: asset.size ?? null, mimeType: asset.mimeType ?? null })
            }
          }
          if (current() && fileCurrent()) {
            latest.current.onFiles([...latest.current.files, ...added])
            handedOff = true
          }
        } catch (cause) { errors.push(cause) }
        finally {
          if (!handedOff) for (const file of owned) {
            try { privateFiles.release(file.uri) } catch (cause) { errors.push(cause) }
          }
          for (const uri of sources) {
            try { privateFiles.releasePicker(uri) } catch (cause) { errors.push(cause) }
          }
          if (errors.length) throw errors.length === 1 ? errors[0] : new AggregateError(errors, '未能添加附件或清理选择副本。')
        }
      })
    } catch (error) {
      if (current() && fileCurrent()) Alert.alert('未能添加附件', error instanceof Error ? error.message : '请重试，或选择其他文件。')
    } finally {
      picking.current = false
    }
  }
  const choose = () => {
    if (picking.current || !latest.current.mounted || !latest.current.enabled) return
    const generation = latest.current.generation, connection = latest.current.device.connectionGeneration()
    if (ios) ActionSheetIOS.showActionSheetWithOptions({ options: ['照片图库', '文件', '取消'], cancelButtonIndex: 2 }, (index) => {
      if (index === 0) void pick('photos', generation, connection)
      if (index === 1) void pick('files', generation, connection)
    })
    else Alert.alert('添加附件', undefined, [
      { text: '照片图库', onPress: () => void pick('photos', generation, connection) },
      { text: '文件', onPress: () => void pick('files', generation, connection) },
      { text: '取消', style: 'cancel' },
    ])
  }
  const attach = <Button variant="ghost" iconOnly accessibilityLabel={files.length ? `添加附件，已选 ${files.length} 个` : '添加附件'} disabled={!enabled} onPress={choose}>
    <Icon as={Paperclip} className="text-secondary" />
  </Button>
  const controls = <View className="flex-row items-center gap-2">
    {onStop ? <Button iconOnly variant="secondary" accessibilityLabel="停止" onPress={onStop}><Icon as={Square} className="fill-foreground" /></Button> : null}
    <Button iconOnly accessibilityLabel={sending ? '本地保存中' : sendLabel} accessibilityState={{ busy: sending }} disabled={!canSend}
      onPress={sending ? undefined : () => { void Haptics.impactAsync(Haptics.ImpactFeedbackStyle.Light); onSend() }}>
      <Icon as={sending ? LoaderCircle : ArrowUp} />
    </Button>
  </View>
  const input = <ComposerInput maxFontSizeMultiplier={2} accessibilityLabel={label} placeholder={blocked ?? '发消息'} multiline maxLength={100000}
    value={value} onChangeText={onChange} />
  const attachments = files.length ? <ScrollView horizontal showsHorizontalScrollIndicator={false} contentContainerClassName="gap-2 px-2 pt-2">
    {files.map((file, index) => <View key={`${file.uri}:${index}`} className="max-w-60 flex-row items-center gap-2 rounded-control bg-surface-muted ps-3">
      <Icon as={Paperclip} className="text-secondary" /><Text numberOfLines={1} className="shrink text-sm">{file.name}</Text>
      <Button variant="ghost" iconOnly accessibilityLabel={`移除 ${file.name}`} onPress={() => { try { privateFiles.release(file.uri); onFiles(files.filter((_, at) => at !== index)) } catch (cause) { Alert.alert('未能移除附件', cause instanceof Error ? cause.message : '请重试。') } }}><Icon as={X} /></Button>
    </View>)}
  </ScrollView> : undefined
  return <View className="gap-2 bg-background px-4 pb-3">
    <View className="w-full max-w-3xl self-center"><ComposerPanel attachments={attachments} editor={input} actions={<><View className="flex-row items-center gap-1">{attach}{target}</View>{controls}</>} /></View>
    {blocked && value || children ? <View className="w-full max-w-3xl gap-2 self-center px-2">
      {blocked && value ? <Text className="text-sm text-muted">{blocked}</Text> : null}
      {children}
    </View> : null}
  </View>
}
