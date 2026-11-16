import type { ComponentProps } from 'react'
import { TextInput } from 'react-native'
import { Input } from './input'
import { cn } from '@/lib/utils'

/** Arc textarea shares input states, with a multiline native editor. */
export function Textarea({ className, multiline = true, numberOfLines = 4, ...props }: ComponentProps<typeof TextInput>) {
  return <Input {...props} multiline={multiline} numberOfLines={numberOfLines} textAlignVertical="top" className={cn('min-h-24 py-3', className)} />
}
