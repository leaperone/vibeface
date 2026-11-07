import * as DocumentPicker from 'expo-document-picker'
import { File } from 'expo-file-system'
import * as Haptics from 'expo-haptics'
import * as ImagePicker from 'expo-image-picker'
import { ArrowUp, LoaderCircle, Paperclip, Square, X } from 'lucide-react-native'
import { useRef, type ReactNode } from 'react'
import { ActionSheetIOS, Alert, Platform, Pressable, ScrollView, TextInput, View } from 'react-native'
import { Button } from '@/components/ui/button'
import { caret } from '@/components/ui/input'
import { Icon } from '@/components/ui/icon'
import { Text } from '@/components/ui/text'
import type { Attachment } from '@/data/types'
import { ChatMaterial, type ChatChrome } from './chrome'

type Props = {
  chrome: ChatChrome
  value: string
  onChange: (value: string) => void
  files: Attachment[]
  onFiles: (files: Attachment[]) => void
  /** Why sending is off, in place of the placeholder, or under the box while it holds a draft; null when it is allowed. */
  blocked: string | null
  sending: boolean
  onSend: () => void
  /** Present while a reply runs and can be cancelled: the send button becomes stop. */
  onStop?: () => void
  /** An actionable blocker that replaces the placeholder and draft note. */
  children?: ReactNode
  /** The bot's accent for the live send button; the neutral fill without it. */
  tint?: { fill: string; ink: string }
}

export function Composer({ chrome, value, onChange, files, onFiles, blocked, sending, onSend, onStop, children, tint }: Props) {
  const ios = Platform.OS === 'ios'
  const latest = useRef({ files, onFiles })
  latest.current = { files, onFiles }
  const picking = useRef(false)
  // While writing a follow-up, keep Send available and move Stop beside it.
  const drafting = value.trim().length > 0 || files.length > 0
  const stop = drafting ? undefined : onStop
  const canSend = !blocked && !sending && drafting
  const armed = canSend
  const pick = async (source: 'photos' | 'files') => {
    if (picking.current) return
    picking.current = true
    try {
      let added: Attachment[]
      if (source === 'photos') {
        const picked = await ImagePicker.launchImageLibraryAsync({ mediaTypes: ['images'], allowsMultipleSelection: true, quality: 1, preferredAssetRepresentationMode: ImagePicker.UIImagePickerPreferredAssetRepresentationMode.Current })
        if (picked.canceled) return
        added = picked.assets.map((asset) => {
          const file = new File(asset.uri)
          return { uri: asset.uri, name: asset.fileName || file.name, size: asset.fileSize ?? file.size, mimeType: asset.mimeType || file.type || null }
        })
      } else {
        const picked = await DocumentPicker.getDocumentAsync({ multiple: true, copyToCacheDirectory: true })
        if (picked.canceled) return
        added = picked.assets.map((asset) => ({ uri: asset.uri, name: asset.name, size: asset.size ?? null, mimeType: asset.mimeType ?? null }))
      }
      latest.current.onFiles([...latest.current.files, ...added])
    } catch (error) {
      Alert.alert('未能添加附件', error instanceof Error ? error.message : '请重试，或选择其他文件。')
    } finally {
      picking.current = false
    }
  }
  const choose = () => {
    if (picking.current) return
    if (ios) ActionSheetIOS.showActionSheetWithOptions({ options: ['照片图库', '文件', '取消'], cancelButtonIndex: 2 }, (index) => {
      if (index === 0) void pick('photos')
      if (index === 1) void pick('files')
    })
    else Alert.alert('添加附件', undefined, [
      { text: '照片图库', onPress: () => void pick('photos') },
      { text: '文件', onPress: () => void pick('files') },
      { text: '取消', style: 'cancel' },
    ])
  }
  const attach = <Button variant="ghost" size="icon" className={ios ? 'size-13 rounded-full' : 'rounded-full'} accessibilityLabel={files.length ? `添加附件，已选 ${files.length} 个` : '添加附件'} onPress={choose}>
    <Icon as={Paperclip} className="text-muted-foreground" />
  </Button>
  const controls = <>
    {drafting && onStop ? <Button instant size="icon" variant="outline" className="rounded-full shadow-none" accessibilityLabel="停止" onPress={onStop}>
      <Icon as={Square} className="fill-foreground" />
    </Button> : null}
    <Button instant size="icon" variant={stop ? 'outline' : 'default'} className="rounded-full shadow-none" style={armed && tint ? { backgroundColor: tint.fill } : undefined} accessibilityLabel={stop ? '停止' : sending ? '发送中' : '发送'} disabled={!stop && !canSend}
      onPress={stop ?? (sending ? undefined : () => { void Haptics.impactAsync(Haptics.ImpactFeedbackStyle.Light); onSend() })}>
      {stop ? <Icon as={Square} className="fill-foreground" /> : sending ? <Icon as={LoaderCircle} /> : <Icon as={ArrowUp} color={armed ? tint?.ink : undefined} />}
    </Button>
  </>
  const input = <TextInput {...caret} maxFontSizeMultiplier={2} accessibilityLabel="给 Bot 发消息" placeholder={children ? '发消息' : blocked ?? '发消息'} placeholderTextColorClassName="accent-muted-foreground" multiline maxLength={100000}
    className={ios ? 'max-h-40 min-h-11 min-w-0 flex-1 ps-3.5 pe-2 py-2.5 font-sans text-body text-foreground' : 'max-h-40 min-h-11 px-4 pt-3 pb-1 font-sans text-body text-foreground'} value={value} onChangeText={onChange} />
  const box = <>
    {files.length ? <ScrollView horizontal showsHorizontalScrollIndicator={false} contentContainerClassName="gap-2 px-3 pt-3">
      {files.map((file, index) => <View key={`${file.uri}:${index}`} className="max-w-48 flex-row items-center rounded-full bg-muted ps-3">
        <Text numberOfLines={1} className="shrink text-meta">{file.name}</Text>
        <Pressable accessibilityRole="button" accessibilityLabel={`移除 ${file.name}`} hitSlop={6} className="size-8 items-center justify-center" onPress={() => onFiles(files.filter((_, at) => at !== index))}>
          <Icon as={X} className="size-3.5" />
        </Pressable>
      </View>)}
    </ScrollView> : null}
    {ios ? <View className="min-h-13 flex-row items-end p-1">
      {input}<View className="flex-row gap-1">{controls}</View>
    </View> : <>
      {input}
      <View className="flex-row items-center justify-between ps-0.5 pe-1.5 pb-1.5">{attach}{controls}</View>
    </>}
  </>
  return <View pointerEvents={ios ? 'box-none' : undefined} className={ios ? 'gap-2 px-4 pb-2' : 'gap-2 bg-background px-4 pb-3'}>
    {ios ? <View pointerEvents="box-none" className="w-full max-w-3xl flex-row items-end gap-2 self-center">
      <ChatMaterial chrome={chrome} className="size-13">{attach}</ChatMaterial>
      <ChatMaterial chrome={chrome} className="min-w-0 flex-1">{box}</ChatMaterial>
    </View> : <View className="w-full max-w-3xl self-center rounded-2xl border border-input bg-card shadow-sm">{box}</View>}
    {blocked && value || children ? <View className={ios ? 'w-full max-w-3xl self-center rounded-xl bg-card py-1' : 'w-full max-w-3xl self-center'}>
      {blocked && value && !children ? <Text className="px-2 text-meta text-muted-foreground">{blocked}</Text> : null}
      {children}
    </View> : null}
  </View>
}
