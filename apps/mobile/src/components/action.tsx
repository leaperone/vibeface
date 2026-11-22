import { InlineError } from '@/components/arc/feedback'
import { Check, CircleAlert, Copy, LoaderCircle, type LucideIcon } from 'lucide-react-native'
import { useState, type ReactNode } from 'react'
import { View } from 'react-native'
import { failureCopyText, failureRef, type FailureRef } from '@vibeface/client/lib/error-ref.ts'
import { messageOf } from '@vibeface/client/lib/node.ts'
import { useClipboardAction } from '@/lib/use-clipboard'
import { Button, type ButtonProps } from '@/components/arc/button'
import { Icon } from '@/components/arc/icon'
import { Text } from '@/components/arc/text'

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
  const failure = failureRef(error)
  return error ? <InlineError>
    <Icon as={CircleAlert} className="mt-0.5 text-danger" />
    <Text className="min-w-0 flex-1 text-sm text-secondary">{error}</Text>
    {failure ? <FailureRefButton failure={failure} /> : null}
  </InlineError> : null
}

/** The failed request's number as `#` and eight characters; a press copies it with its time and conversation. */
function FailureRefButton({ failure }: { failure: FailureRef }) {
  const { busy, copied, copy } = useClipboardAction(failure)
  return <Button variant="ghost" size="sm" className="-my-2 -me-2" accessibilityLabel={`复制错误编号 ${failure.ref}`} disabled={busy} onPress={() => void copy(failureCopyText(failure))}>
    <Icon as={copied ? Check : Copy} className={copied ? 'size-4 text-success' : 'size-4'} /><Text className="font-mono">#{failure.ref}</Text>
  </Button>
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
