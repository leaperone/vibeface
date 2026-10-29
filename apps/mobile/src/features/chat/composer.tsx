import * as DocumentPicker from 'expo-document-picker'
import * as Haptics from 'expo-haptics'
import { ArrowUp, Paperclip, Square, X } from 'lucide-react-native'
import type { ReactNode } from 'react'
import { Pressable, ScrollView, TextInput, View } from 'react-native'
import { Button } from '@/components/ui/button'
import { caret } from '@/components/ui/input'
import { Icon } from '@/components/ui/icon'
import { Text } from '@/components/ui/text'
import type { Attachment } from '@/data/types'

type Props = {
  value: string
  onChange: (value: string) => void
  files: Attachment[]
  onFiles: (files: Attachment[]) => void
  /** Why sending is off, in place of the placeholder, or under the box while it holds a draft; null when it is allowed. */
  blocked: string | null
  /** A message is on its way: the button rests, and nothing below the box appears and shifts it. */
  sending: boolean
  onSend: () => void
  /** Present while a reply runs and can be cancelled: the send button becomes stop. */
  onStop?: () => void
  /** A follow-up under the blocked note, kept on the composer's column. */
  children?: ReactNode
  /** The bot's accent for the live send button; the neutral fill without it. */
  tint?: { fill: string; ink: string }
}

export function Composer({ value, onChange, files, onFiles, blocked, sending, onSend, onStop, children, tint }: Props) {
  const canSend = !blocked && !sending && value.trim().length > 0
  const armed = canSend && !onStop
  const pick = async () => {
    const picked = await DocumentPicker.getDocumentAsync({ multiple: true, copyToCacheDirectory: true })
    if (picked.canceled) return
    onFiles([...files, ...picked.assets.map((asset) => ({ uri: asset.uri, name: asset.name, size: asset.size ?? null, mimeType: asset.mimeType ?? null }))])
  }
  return <View className="gap-2 bg-background px-4 pb-3">
    <View className="w-full max-w-3xl self-center rounded-2xl border border-input bg-card shadow-sm">
      {files.length ? <ScrollView horizontal showsHorizontalScrollIndicator={false} contentContainerClassName="gap-2 px-3 pt-3">
        {files.map((file, index) => <View key={`${file.uri}:${index}`} className="max-w-48 flex-row items-center rounded-full bg-muted ps-3">
          <Text numberOfLines={1} className="shrink text-meta">{file.name}</Text>
          <Pressable accessibilityRole="button" accessibilityLabel={`移除 ${file.name}`} hitSlop={6} className="size-8 items-center justify-center" onPress={() => onFiles(files.filter((_, at) => at !== index))}>
            <Icon as={X} className="size-3.5" />
          </Pressable>
        </View>)}
      </ScrollView> : null}
      <TextInput {...caret} maxFontSizeMultiplier={2} accessibilityLabel="给 Bot 发消息" placeholder={blocked ?? '发消息'} placeholderTextColorClassName="accent-muted-foreground" multiline maxLength={100000}
        className="max-h-40 min-h-11 px-4 pt-3 pb-1 font-sans text-body text-foreground" value={value} onChangeText={onChange} />
      <View className="flex-row items-center justify-between ps-0.5 pe-1.5 pb-1.5">
        <Button variant="ghost" size="icon" className="rounded-full" accessibilityLabel={files.length ? `添加附件，已选 ${files.length} 个` : '添加附件'} onPress={pick}>
          <Icon as={Paperclip} className="text-muted-foreground" />
        </Button>
        {/* Idle, the disabled default button rests muted; ready, it takes the bot's tint. */}
        <Button instant size="icon" variant={onStop || sending ? 'outline' : 'default'} className="rounded-full shadow-none" style={armed && tint ? { backgroundColor: tint.fill } : undefined} accessibilityLabel={onStop || sending ? '停止' : '发送'} disabled={!onStop && !sending && !canSend}
          // While the send goes out the stop look holds at full strength and ignores taps, so the button changes once.
          onPress={onStop ?? (sending ? undefined : () => { void Haptics.impactAsync(Haptics.ImpactFeedbackStyle.Light); onSend() })}>
          {onStop || sending ? <Icon as={Square} className="fill-foreground" /> : <Icon as={ArrowUp} color={armed ? tint?.ink : undefined} />}
        </Button>
      </View>
    </View>
    {blocked && value || children ? <View className="w-full max-w-3xl self-center">
      {blocked && value ? <Text className="px-2 text-meta text-muted-foreground">{blocked}</Text> : null}
      {children}
    </View> : null}
  </View>
}
