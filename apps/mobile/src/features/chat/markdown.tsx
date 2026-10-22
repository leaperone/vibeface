import { useMemo } from 'react'
import { View } from 'react-native'
import { useMarkdown, type MarkedStyles } from 'react-native-marked'
import { useCSSVariable, useUniwind } from 'uniwind'

/** A bot's reply, styled with the web's `.md` sizes and the shared colour tokens. */
export function Markdown({ text }: { text: string }) {
  const { theme } = useUniwind()
  const [foreground, muted, border, mutedForeground] = useCSSVariable(['--foreground', '--muted', '--border', '--muted-foreground']) as string[]
  const styles = useMemo<MarkedStyles>(() => {
    const body = { fontFamily: 'Geist', fontSize: 15, lineHeight: 24, color: foreground }
    const heading = { ...body, fontSize: 16, fontWeight: '600' as const, marginTop: 4 }
    return {
      text: body, paragraph: { marginVertical: 0, paddingVertical: 6 }, li: body, strong: { ...body, fontWeight: '600' },
      h1: heading, h2: heading, h3: heading, h4: heading,
      link: { ...body, textDecorationLine: 'underline' },
      codespan: { fontFamily: 'Geist Mono', fontSize: 13.5, backgroundColor: muted, color: foreground },
      code: { backgroundColor: muted, borderColor: border, borderWidth: 1, borderRadius: 12, padding: 12, marginVertical: 6 },
      codeText: { fontFamily: 'Geist Mono', fontSize: 13, lineHeight: 20, color: foreground },
      blockquote: { borderLeftColor: border, borderLeftWidth: 3, paddingLeft: 12 },
      hr: { backgroundColor: border },
      table: { borderColor: border }, tableCell: { borderColor: border },
      em: { ...body, fontStyle: 'italic', color: mutedForeground },
    }
  }, [foreground, muted, border, mutedForeground])
  const nodes = useMarkdown(text, {
    colorScheme: theme === 'dark' ? 'dark' : 'light',
    theme: { colors: { text: foreground, link: foreground, border, code: muted } },
    styles,
  })
  return <View className="min-w-0">{nodes}</View>
}
