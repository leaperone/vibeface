import { CircleAlert, LoaderCircle, type LucideIcon } from 'lucide-react-native'
import { useState, type ReactNode } from 'react'
import { View } from 'react-native'
import { messageOf } from '@vibeface/client/lib/node.ts'
import { Button, type ButtonProps } from '@/components/ui/button'
import { Icon } from '@/components/ui/icon'
import { Text } from '@/components/ui/text'

/** One action at a time per screen part: which one runs, and why the last one failed. */
export function useAction() {
  const [busy, setBusy] = useState<string | null>(null)
  const [error, setError] = useState('')
  const run = async (key: string, action: () => Promise<unknown>) => {
    if (busy) return
    setBusy(key)
    setError('')
    try { await action() } catch (caught) { setError(messageOf(caught)) } finally { setBusy(null) }
  }
  return { busy, error, run, clear: () => setError('') }
}

export function ErrorText({ error }: { error: string }) {
  return error ? <View role="alert" accessible className="flex-row items-start gap-2">
    <Icon as={CircleAlert} className="mt-0.5 text-danger" />
    <Text className="min-w-0 flex-1 text-meta text-danger">{error}</Text>
  </View> : null
}

/** A button that runs its action, spins while it runs and shows why it failed under it. */
export function ActionButton({ icon, iconClassName, children, action, disabled, className, ...props }: Omit<ButtonProps, 'children' | 'onPress'> & {
  icon?: LucideIcon; iconClassName?: string; children: ReactNode; action: () => Promise<unknown>
}) {
  const { busy, error, run } = useAction()
  return <View className="gap-1">
    <Button {...props} className={className} disabled={disabled || busy !== null} onPress={() => void run('run', action)}>
      {busy ? <Icon as={LoaderCircle} /> : icon ? <Icon as={icon} className={iconClassName} /> : null}
      <Text>{children}</Text>
    </Button>
    <ErrorText error={error} />
  </View>
}
