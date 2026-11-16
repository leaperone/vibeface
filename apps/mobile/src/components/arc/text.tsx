import { createContext, useContext, type ComponentProps } from 'react'
import { Text as NativeText } from 'react-native'
import { cn } from '@/lib/utils'

export const TextClassContext = createContext<string | undefined>(undefined)

/** Arc body typography. Native text retains Dynamic Type and selectable text. */
export function Text({ className, maxFontSizeMultiplier = 2, ...props }: ComponentProps<typeof NativeText>) {
  const inherited = useContext(TextClassContext)
  return <NativeText className={cn('font-sans text-base font-normal text-foreground', inherited, className)} maxFontSizeMultiplier={maxFontSizeMultiplier} {...props} />
}
