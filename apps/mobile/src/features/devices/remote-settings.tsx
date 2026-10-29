import { FolderOpen, Plus, X } from 'lucide-react-native'
import { useEffect, useRef, useState, type ReactNode } from 'react'
import { View } from 'react-native'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { messageOf } from '@vibeface/client/lib/node.ts'
import { ErrorText } from '@/components/action'
import { Button } from '@/components/ui/button'
import { Icon } from '@/components/ui/icon'
import { Input } from '@/components/ui/input'
import { Text } from '@/components/ui/text'

/** Calls the connected device; a newer call or a new connection makes an older answer stale. */
export function useCall<T>(parse: (value: unknown) => T, loadMethod: string) {
  const { ready } = useDevice()
  const [value, setValue] = useState<T | null>(null)
  const [busy, setBusy] = useState('')
  const [error, setError] = useState<{ at: string; text: string } | null>(null)
  const sequence = useRef(0)
  const call = async (key: string, method: string, params?: Record<string, unknown>, at = '') => {
    if (!ready?.isCurrent()) return null
    const connection = ready, owner = ++sequence.current
    setBusy(key); setError(null)
    try {
      const result = await connection.request(method, params)
      if (owner !== sequence.current || !connection.isCurrent()) return null
      setValue(parse(method === loadMethod || method.startsWith('configure') ? result : await connection.request(loadMethod)))
      return result
    } catch (cause) {
      if (owner === sequence.current) setError({ at, text: messageOf(cause) })
      return null
    } finally { if (owner === sequence.current) setBusy('') }
  }
  const load = () => call('load', loadMethod)
  useEffect(() => { setValue(null); void load(); return () => { sequence.current += 1 } }, [ready])
  return { value, busy, error, call, load, setError }
}

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
    {items.map((item) => <View key={item} className="min-h-11 flex-row items-center gap-2 rounded-lg bg-muted/60 ps-3">
      <Icon as={FolderOpen} className="size-4 text-muted-foreground" />
      <Text numberOfLines={1} ellipsizeMode="head" className="min-w-0 flex-1 font-mono text-meta">{shown[item] ?? item}</Text>
      {trailing?.(item)}
      <Button variant="ghost" size="icon" accessibilityLabel={`移除 ${shown[item] ?? item}`} disabled={busy} onPress={() => onChange(items.filter((other) => other !== item))}><Icon as={X} /></Button>
    </View>)}
    {suggested.length ? <View className="flex-row flex-wrap gap-2">{suggested.map((folder) => <Button key={folder.path} variant="outline" accessibilityLabel={`添加 ${folder.label}`} disabled={busy} onPress={() => onChange([...items, folder.path])}>
      <Icon as={Plus} /><Text className="font-mono text-meta">{folder.label}</Text>
    </Button>)}</View> : null}
    <View className="flex-row gap-2">
      <Input accessibilityLabel="文件夹路径" placeholder={items.length || suggested.length ? '其他文件夹' : '~/projects'} autoCapitalize="none" autoCorrect={false} className="min-w-0 flex-1 font-mono" value={path} editable={!busy}
        returnKeyType="done" onChangeText={setPath} onSubmitEditing={add} />
      <Button variant="outline" disabled={busy || !path.trim()} onPress={add}><Icon as={Plus} /><Text>添加</Text></Button>
    </View>
  </>
  return label ? <Field label={label}>{body}</Field> : <View className="gap-2">{body}</View>
}

export function Field({ label, children }: { label: string; children: ReactNode }) {
  return <View className="gap-2">
    <Text className="text-meta font-medium text-muted-foreground">{label}</Text>
    {children}
  </View>
}

export function Retry({ error, onRetry }: { error: string; onRetry(): void }) {
  return <View className="gap-2"><ErrorText error={error} /><Button variant="outline" className="self-start" onPress={onRetry}><Text>重试</Text></Button></View>
}
