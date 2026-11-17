import type { Attachment } from '@vibeface/client/chat/types.ts'
import { sha256Hex } from '@vibeface/client/lib/byte-codec.ts'
import { FileText, MoreHorizontal, Paperclip, X } from 'lucide-react-native'
import { useEffect, useRef, useState } from 'react'
import { ActionSheetIOS, Alert, Image, Modal, Platform, View } from 'react-native'
import { SafeAreaView } from 'react-native-safe-area-context'
import { Button } from '@/components/arc/button'
import { Icon } from '@/components/arc/icon'
import { Text } from '@/components/arc/text'
import { attachmentSize, validAttachment, type ReadAttachment } from '@/platform/chat-attachments'
import { localAttachment, shareAttachment, type LocalAttachment } from '@/platform/native-chat-attachments'

export type { ReadAttachment } from '@/platform/chat-attachments'
export function MessageAttachments({ files, scope, read }: { files: Attachment[]; scope?: string; read?: ReadAttachment }) {
  return <View className="gap-2">{files.map((file, index) => <MessageAttachment key={JSON.stringify([scope, file.hash, file.size, file.name, index])} file={file} scope={scope} read={read} />)}</View>
}
function MessageAttachment({ file, scope, read }: { file: Attachment; scope?: string; read?: ReadAttachment }) {
  const [local, setLocal] = useState<LocalAttachment | null>(null)
  const [state, setState] = useState<{ kind: 'idle' } | { kind: 'loading'; received: number } | { kind: 'failed'; message: string }>({ kind: 'idle' })
  const [preview, setPreview] = useState(false)
  const generation = useRef(0), busy = useRef(false), active = useRef(true)
  const identity = JSON.stringify([scope, file.hash, file.size, file.name])
  useEffect(() => {
    active.current = true
    const current = ++generation.current
    setLocal(null); setPreview(false); setState({ kind: 'idle' }); busy.current = false
    if (scope && validAttachment(file)) void sha256Hex(new TextEncoder().encode(scope)).then(key => localAttachment(key, file)).then(value => {
      if (generation.current === current) setLocal(value)
    }).catch(() => {})
    return () => { active.current = false; generation.current++ }
  }, [identity])
  const perform = async (action: 'open' | 'save' | 'share') => {
    if (busy.current || !active.current) return
    busy.current = true
    const current = generation.current
    setState({ kind: 'loading', received: 0 })
    try {
      if (!scope) throw new Error('请回到原对话打开附件。')
      const key = await sha256Hex(new TextEncoder().encode(scope))
      const downloaded = await localAttachment(key, file, read, received => { if (generation.current === current) setState({ kind: 'loading', received }) })
      if (generation.current !== current) return
      setLocal(downloaded)
      setState({ kind: 'idle' })
      if (action === 'open' && downloaded.image) setPreview(true)
      else await shareAttachment(downloaded, action === 'save' ? `保存 ${file.name}` : action === 'open' ? `打开 ${file.name}` : `分享 ${file.name}`)
    } catch (cause) {
      if (generation.current === current) setState({ kind: 'failed', message: cause instanceof Error ? cause.message : '附件无法打开，请重试。' })
    } finally { if (generation.current === current) busy.current = false }
  }
  const choose = () => {
    const actions = [() => void perform('save'), () => void perform('share')]
    if (Platform.OS === 'ios') ActionSheetIOS.showActionSheetWithOptions({ title: file.name, options: ['保存副本', '分享附件', '取消'], cancelButtonIndex: 2 }, index => actions[index]?.())
    else Alert.alert(file.name, '在系统面板中选择保存位置或接收应用。', [{ text: '保存副本', onPress: actions[0] }, { text: '分享附件', onPress: actions[1] }, { text: '取消', style: 'cancel' }])
  }
  const loading = state.kind === 'loading'
  return <View className="gap-1">
    <View className="flex-row items-center gap-1 rounded-control border border-border bg-surface-muted p-1">
      <Button variant="ghost" className="min-w-0 shrink grow justify-start gap-2 px-2" disabled={loading || !validAttachment(file)} accessibilityLabel={`打开附件 ${file.name}，${attachmentSize(file.size)}`} accessibilityState={{ busy: loading }} onPress={() => void perform('open')}>
        {local?.image ? <Image source={{ uri: local.uri }} accessibilityIgnoresInvertColors className="h-12 w-12 rounded-control" onError={() => { setLocal({ ...local, image: false }); setState({ kind: 'failed', message: '图片无法预览，仍可保存或分享附件。' }) }} /> : <Icon as={FileText} className="text-secondary" />}
        <View className="min-w-0 shrink gap-1"><Text numberOfLines={2} className="text-sm text-foreground">{file.name}</Text><Text className="text-sm text-muted">{loading ? `下载中 ${attachmentSize(state.received)} / ${attachmentSize(file.size)}` : `${attachmentSize(file.size)} · ${local ? '已保留副本' : '点按打开'}`}</Text></View>
      </Button>
      <Button iconOnly variant="ghost" disabled={loading || !validAttachment(file)} accessibilityLabel={`保存或分享 ${file.name}`} onPress={choose}><Icon as={MoreHorizontal} /></Button>
    </View>
    {state.kind === 'failed' ? <View accessibilityLiveRegion="polite" className="gap-1"><Text className="text-sm text-danger">{state.message}</Text><Button variant="ghost" size="sm" onPress={() => void perform('open')}><Icon as={Paperclip} /><Text>重新打开</Text></Button></View> : null}
    <Modal visible={preview} animationType="none" onRequestClose={() => setPreview(false)} presentationStyle="fullScreen">
      <SafeAreaView className="flex-1 bg-background"><View className="flex-row items-center justify-between gap-2 px-4"><Text numberOfLines={1} className="shrink">{file.name}</Text><Button variant="ghost" iconOnly accessibilityLabel="关闭图片" onPress={() => setPreview(false)}><Icon as={X} /></Button></View>
        {local ? <Image source={{ uri: local.uri }} resizeMode="contain" accessibilityLabel={file.name} accessibilityIgnoresInvertColors className="flex-1" onError={() => { setPreview(false); setState({ kind: 'failed', message: '图片无法预览，仍可保存或分享附件。' }) }} /> : null}
        <View className="flex-row justify-center gap-2 p-4"><Button variant="secondary" onPress={() => void perform('save')} disabled={loading}><Text>保存副本</Text></Button><Button variant="secondary" onPress={() => void perform('share')} disabled={loading}><Text>分享</Text></Button></View>
      </SafeAreaView>
    </Modal>
  </View>
}
