import { useMemo } from 'react'
import { View, type ViewStyle } from 'react-native'
import { Renderer, useMarkdown, type MarkedStyles } from 'react-native-marked'
import { useCSSVariable, useUniwind } from 'uniwind'

/** A bot's reply, styled with the web's `.md` sizes and the shared colour tokens. */
export function Markdown({ text }: { text: string }) {
  const { theme } = useUniwind()
  const [foreground, muted, border, mutedForeground] = useCSSVariable(['--foreground', '--muted', '--border', '--muted-foreground']) as string[]
  const styles = useMemo<MarkedStyles>(() => {
    const body = { fontFamily: 'Geist', fontSize: 15, lineHeight: 24, color: foreground }
    const heading = { ...body, fontSize: 16, fontWeight: '600' as const, marginTop: 4 }
    return {
      text: body, paragraph: { marginVertical: 0, paddingVertical: 6 }, li: { ...body, fontVariant: ['tabular-nums'] }, strong: { ...body, fontWeight: '600' },
      h1: { ...heading, fontSize: 20, lineHeight: 28, marginTop: 12 }, h2: { ...heading, fontSize: 17, lineHeight: 26, marginTop: 12 }, h3: heading, h4: heading,
      link: { ...body, textDecorationLine: 'underline' },
      codespan: { fontFamily: 'Geist Mono', fontSize: 13.5, fontStyle: 'normal', fontWeight: '400', backgroundColor: muted, color: foreground },
      code: { padding: 12 },
      codeText: { fontFamily: 'Geist Mono', fontSize: 13, lineHeight: 20, color: foreground },
      blockquote: { borderLeftColor: border, borderLeftWidth: 3, paddingLeft: 12 },
      hr: { backgroundColor: border },
      table: { borderColor: border }, tableCell: { borderColor: border },
      em: { ...body, fontStyle: 'italic', color: mutedForeground },
    }
  }, [foreground, muted, border, mutedForeground])
  const renderer = useMemo(() => new FramedCode({ backgroundColor: muted, borderColor: border, borderWidth: 1, borderRadius: 12, marginVertical: 6, overflow: 'hidden' }), [muted, border])
  const nodes = useMarkdown(text, {
    renderer,
    colorScheme: theme === 'dark' ? 'dark' : 'light',
    theme: { colors: { text: foreground, link: foreground, border, code: muted } },
    styles,
  })
  return <View className="min-w-0">{nodes}</View>
}

/** The library frames the scrolled content, so a long line ran off the frame; this frames the visible window instead. */
class FramedCode extends Renderer {
  constructor(private frame: ViewStyle) { super() }
  code(...args: Parameters<Renderer['code']>) {
    return <View key={this.getKey()} style={this.frame}>{super.code(...args)}</View>
  }
}
