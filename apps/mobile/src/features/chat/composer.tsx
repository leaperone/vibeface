import * as DocumentPicker from 'expo-document-picker'
import { ArrowUp, Paperclip, Square, X } from 'lucide-react-native'
import { Pressable, ScrollView, TextInput, View } from 'react-native'
import { Button } from '@/components/ui/button'
import { Icon } from '@/components/ui/icon'
import { Text } from '@/components/ui/text'
import type { Attachment } from '@/data/types'

type Props = {
  value: string
  onChange: (value: string) => void
  files: Attachment[]
  onFiles: (files: Attachment[]) => void
  /** Why sending is off, shown under the box; null when it is allowed. */
  blocked: string | null
  onSend: () => void
  /** Present while a reply runs and can be cancelled: the send button becomes stop. */
  onStop?: () => void
}

export function Composer({ value, onChange, files, onFiles, blocked, onSend, onStop }: Props) {
  const canSend = !blocked && value.trim().length > 0
  const pick = async () => {
    const picked = await DocumentPicker.getDocumentAsync({ multiple: true, copyToCacheDirectory: true })
    if (picked.canceled) return
    onFiles([...files, ...picked.assets.map((asset) => ({ uri: asset.uri, name: asset.name, size: asset.size ?? null, mimeType: asset.mimeType ?? null }))])
  }
  return <View className="gap-2 bg-background px-4 pt-2 pb-3">
    <View className="w-full max-w-[736px] self-center rounded-3xl border border-border bg-card">
      {files.length ? <ScrollView horizontal showsHorizontalScrollIndicator={false} contentContainerClassName="gap-2 px-3 pt-3">
        {files.map((file, index) => <View key={`${file.uri}:${index}`} className="max-w-48 flex-row items-center rounded-full bg-muted ps-3">
          <Text numberOfLines={1} className="shrink text-meta">{file.name}</Text>
          <Pressable accessibilityRole="button" accessibilityLabel={`移除 ${file.name}`} hitSlop={6} className="size-8 items-center justify-center" onPress={() => onFiles(files.filter((_, at) => at !== index))}>
            <Icon as={X} className="size-3.5" />
          </Pressable>
        </View>)}
      </ScrollView> : null}
      <TextInput maxFontSizeMultiplier={2} accessibilityLabel="给 bot 发消息" placeholder="发消息" placeholderTextColorClassName="accent-muted-foreground" multiline maxLength={100000}
        className="max-h-40 min-h-11 px-4 pt-3 pb-1 font-sans text-base text-foreground" value={value} onChangeText={onChange} />
      <View className="flex-row items-center justify-between px-1.5 pb-1.5">
        <Button variant="ghost" size="icon" className="size-11 rounded-full" accessibilityLabel={files.length ? `添加附件，已选 ${files.length} 个` : '添加附件'} onPress={pick}>
          <Icon as={Paperclip} />
        </Button>
        {onStop
          ? <Button variant="outline" size="icon" className="size-11 rounded-full" accessibilityLabel="停止" onPress={onStop}><Icon as={Square} className="size-4 fill-foreground" /></Button>
          : <Button size="icon" variant={canSend ? 'default' : 'secondary'} className="size-11 rounded-full" accessibilityLabel="发送" disabled={!canSend} onPress={onSend}><Icon as={ArrowUp} className={canSend ? undefined : 'text-muted-foreground'} /></Button>}
      </View>
    </View>
    {blocked ? <Text className="px-2 text-meta text-muted-foreground">{blocked}</Text> : null}
  </View>
}
