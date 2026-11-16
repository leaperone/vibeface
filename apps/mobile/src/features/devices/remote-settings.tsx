import { FolderOpen, Plus, X } from 'lucide-react-native'
import { useState, type ReactNode } from 'react'
import { View } from 'react-native'
import { ErrorText } from '@/components/action'
import { Button } from '@/components/arc/button'
import { Icon } from '@/components/arc/icon'
import { Input } from '@/components/arc/input'
import { Text } from '@/components/arc/text'

/**
 * Folder paths with a remove button each, one-tap folders the device found (`suggested`) and a field to add one; a long
 * path keeps its last folder readable. `shown`: a path's `~/…` form.
 */
export function PathList({ label, items, shown = {}, suggested = [], busy, trailing, onChange }: {
  label: string; items: string[]; shown?: Record<string, string>; suggested?: { path: string; label: string }[]; busy: boolean; trailing?: (item: string) => ReactNode; onChange(next: string[]): void
}) {
  const [path, setPath] = useState('')
  const add = () => { const next = path.trim(); setPath(''); if (next && !items.includes(next)) onChange([...items, next]) }
  const body = <>
    {items.map((item) => <View key={item} className="min-h-11 flex-row items-center gap-2 rounded-control bg-surface-muted/60 ps-3">
      <Icon as={FolderOpen} className="size-4 text-muted" />
      <Text numberOfLines={1} ellipsizeMode="head" className="min-w-0 flex-1 font-mono text-sm">{shown[item] ?? item}</Text>
      {trailing?.(item)}
      <Button variant="ghost" iconOnly accessibilityLabel={`移除 ${shown[item] ?? item}`} disabled={busy} onPress={() => onChange(items.filter((other) => other !== item))}><Icon as={X} /></Button>
    </View>)}
    {suggested.length ? <View className="flex-row flex-wrap gap-2">{suggested.map((folder) => <Button key={folder.path} variant="secondary" accessibilityLabel={`添加 ${folder.label}`} disabled={busy} onPress={() => onChange([...items, folder.path])}>
      <Icon as={Plus} /><Text className="font-mono text-sm">{folder.label}</Text>
    </Button>)}</View> : null}
    <View className="flex-row gap-2">
      <Input accessibilityLabel="文件夹路径" placeholder={items.length || suggested.length ? '其他文件夹' : '~/projects'} autoCapitalize="none" autoCorrect={false} className="min-w-0 flex-1 font-mono" value={path} editable={!busy}
        returnKeyType="done" onChangeText={setPath} onSubmitEditing={add} />
      <Button variant="secondary" disabled={busy || !path.trim()} onPress={add}><Icon as={Plus} /><Text>添加</Text></Button>
    </View>
  </>
  return label ? <Field label={label}>{body}</Field> : <View className="gap-2">{body}</View>
}

export function Field({ label, children }: { label: string; children: ReactNode }) {
  return <View className="gap-2">
    <Text className="text-sm font-medium text-muted">{label}</Text>
    {children}
  </View>
}

export function Retry({ error, onRetry }: { error: string; onRetry(): void }) {
  return <View className="gap-2"><ErrorText error={error} /><Button variant="secondary" className="self-start" onPress={onRetry}><Text>重试</Text></Button></View>
}
