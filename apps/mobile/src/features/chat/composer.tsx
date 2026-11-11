import * as DocumentPicker from 'expo-document-picker'
import { File } from 'expo-file-system'
import * as Haptics from 'expo-haptics'
import * as ImagePicker from 'expo-image-picker'
import { ArrowUp, LoaderCircle, Paperclip, Square, X } from 'lucide-react-native'
import { useEffect, useRef, type ReactNode } from 'react'
import { ActionSheetIOS, Alert, Platform, Pressable, ScrollView, TextInput, View } from 'react-native'
import { Button } from '@/components/ui/button'
import { caret } from '@/components/ui/input'
import { Icon } from '@/components/ui/icon'
import { Text } from '@/components/ui/text'
import type { Attachment } from '@/data/types'
import { privateFiles } from '@/platform/files'
import { ChatMaterial, type ChatChrome } from './chrome'

type Props = {
  chrome: ChatChrome
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
  /** The bot's accent for the live send button; the neutral fill without it. */
  tint?: { fill: string; ink: string }
  /** A small control beside the send button, inside the box. */
  target?: ReactNode
  sendLabel?: string
  label?: string
}

export function Composer({ chrome, value, onChange, files, onFiles, attachmentScope, blocked, sending, onSend, onStop, children, tint, target, sendLabel = '发送', label = '给 Bot 发消息' }: Props) {
  const ios = Platform.OS === 'ios'
  const enabled = !blocked && !sending
  const latest = useRef({ files, onFiles, attachmentScope, enabled, generation: 0, mounted: true })
  if (latest.current.attachmentScope !== attachmentScope || latest.current.enabled !== enabled) latest.current.generation++
  latest.current = { ...latest.current, files, onFiles, attachmentScope, enabled }
  useEffect(() => {
    latest.current.mounted = true
    return () => { latest.current.mounted = false; latest.current.generation++ }
  }, [])
  const picking = useRef(false)
  const drafting = value.trim().length > 0 || files.length > 0
  const canSend = !blocked && !sending && drafting
  const armed = canSend
  const pick = async (source: 'photos' | 'files', generation: number) => {
    if (picking.current || !latest.current.mounted || !latest.current.enabled || latest.current.generation !== generation) return
    picking.current = true
    const scope = latest.current.attachmentScope, request = ++latest.current.generation
    const current = () => latest.current.mounted && latest.current.enabled && latest.current.attachmentScope === scope && latest.current.generation === request
    try {
      await privateFiles.run(async (fileCurrent) => {
        const sources: string[] = [], added: Attachment[] = []
        try {
          if (source === 'photos') {
            const picked = await ImagePicker.launchImageLibraryAsync({ mediaTypes: ['images'], allowsMultipleSelection: true, quality: 1, preferredAssetRepresentationMode: ImagePicker.UIImagePickerPreferredAssetRepresentationMode.Current })
            if (picked.canceled) return
            sources.push(...picked.assets.map(asset => asset.uri))
            if (!current() || !fileCurrent()) return
            for (const asset of picked.assets) {
              const file = new File(asset.uri)
              added.push({ uri: privateFiles.retain(asset.uri).uri, name: asset.fileName || file.name, size: asset.fileSize ?? file.size, mimeType: asset.mimeType || file.type || null })
            }
          } else {
            const picked = await DocumentPicker.getDocumentAsync({ multiple: true, copyToCacheDirectory: true })
            if (picked.canceled) return
            sources.push(...picked.assets.map(asset => asset.uri))
            if (!current() || !fileCurrent()) return
            for (const asset of picked.assets) added.push({ uri: privateFiles.retain(asset.uri).uri, name: asset.name, size: asset.size ?? null, mimeType: asset.mimeType ?? null })
          }
          if (current()) latest.current.onFiles([...latest.current.files, ...added])
          else for (const file of added) privateFiles.release(file.uri)
        } catch (cause) { for (const file of added) privateFiles.release(file.uri); throw cause }
        finally { for (const uri of sources) privateFiles.releasePicker(uri) }
      })
    } catch (error) {
      if (current()) Alert.alert('未能添加附件', error instanceof Error ? error.message : '请重试，或选择其他文件。')
    } finally {
      picking.current = false
    }
  }
  const choose = () => {
    if (picking.current || !latest.current.mounted || !latest.current.enabled) return
    const generation = latest.current.generation
    if (ios) ActionSheetIOS.showActionSheetWithOptions({ options: ['照片图库', '文件', '取消'], cancelButtonIndex: 2 }, (index) => {
      if (index === 0) void pick('photos', generation)
      if (index === 1) void pick('files', generation)
    })
    else Alert.alert('添加附件', undefined, [
      { text: '照片图库', onPress: () => void pick('photos', generation) },
      { text: '文件', onPress: () => void pick('files', generation) },
      { text: '取消', style: 'cancel' },
    ])
  }
  const attach = <Button variant="ghost" size="icon" className={ios ? 'size-13 rounded-full' : 'rounded-full'} accessibilityLabel={files.length ? `添加附件，已选 ${files.length} 个` : '添加附件'} disabled={!enabled} onPress={choose}>
    <Icon as={Paperclip} className="text-muted-foreground" />
  </Button>
  const controls = <>
    {onStop ? <Button instant size="icon" variant="outline" className="rounded-full shadow-none" accessibilityLabel="停止" onPress={onStop}>
      <Icon as={Square} className="fill-foreground" />
    </Button> : null}
    <Button instant size="icon" variant="default" className="rounded-full shadow-none" style={armed && tint ? { backgroundColor: tint.fill } : undefined} accessibilityLabel={sending ? '本地保存中' : sendLabel} disabled={!canSend}
      onPress={sending ? undefined : () => { void Haptics.impactAsync(Haptics.ImpactFeedbackStyle.Light); onSend() }}>
      {sending ? <Icon as={LoaderCircle} /> : <Icon as={ArrowUp} color={armed ? tint?.ink : undefined} />}
    </Button>
  </>
  const input = <TextInput {...caret} maxFontSizeMultiplier={2} accessibilityLabel={label} placeholder={blocked ?? '发消息'} placeholderTextColorClassName="accent-muted-foreground" multiline maxLength={100000}
    className={ios ? 'max-h-40 min-h-11 min-w-0 flex-1 ps-3.5 pe-2 py-2.5 font-sans text-body text-foreground' : 'max-h-40 min-h-11 px-4 pt-3 pb-1 font-sans text-body text-foreground'} value={value} onChangeText={onChange} />
  const box = <>
    {files.length ? <ScrollView horizontal showsHorizontalScrollIndicator={false} contentContainerClassName="gap-2 px-3 pt-3">
      {files.map((file, index) => <View key={`${file.uri}:${index}`} className="max-w-48 flex-row items-center rounded-full bg-muted ps-3">
        <Text numberOfLines={1} className="shrink text-meta">{file.name}</Text>
        <Pressable accessibilityRole="button" accessibilityLabel={`移除 ${file.name}`} hitSlop={6} className="size-8 items-center justify-center" onPress={() => { try { privateFiles.release(file.uri); onFiles(files.filter((_, at) => at !== index)) } catch (cause) { Alert.alert('未能移除附件', cause instanceof Error ? cause.message : '请重试。') } }}>
          <Icon as={X} className="size-3.5" />
        </Pressable>
      </View>)}
    </ScrollView> : null}
    {ios ? <View className="min-h-13 flex-row items-end p-1">
      {input}<View className="flex-row gap-1">{target}{controls}</View>
    </View> : <>
      {input}
      <View className="flex-row items-center justify-between ps-0.5 pe-1.5 pb-1.5"><View className="flex-row items-center">{attach}{target}</View>{controls}</View>
    </>}
  </>
  return <View pointerEvents={ios ? 'box-none' : undefined} className={ios ? 'gap-2 px-4 pb-2' : 'gap-2 bg-background px-4 pb-3'}>
    {ios ? <View pointerEvents="box-none" className="w-full max-w-3xl flex-row items-end gap-2 self-center">
      <ChatMaterial chrome={chrome} className="size-13">{attach}</ChatMaterial>
      <ChatMaterial chrome={chrome} className="min-w-0 flex-1">{box}</ChatMaterial>
    </View> : <View className="w-full max-w-3xl self-center rounded-2xl border border-input bg-card shadow-sm">{box}</View>}
    {blocked && value || children ? <View className={ios ? 'w-full max-w-3xl self-center rounded-xl bg-card py-1' : 'w-full max-w-3xl self-center'}>
      {blocked && value ? <Text className="px-2 text-meta text-muted-foreground">{blocked}</Text> : null}
      {children}
    </View> : null}
  </View>
}
