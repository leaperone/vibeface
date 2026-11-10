import { cloneElement, isValidElement, useMemo, type ReactElement, type ReactNode } from 'react'
import { ScrollView, View, type TextProps, type TextStyle, type ViewStyle } from 'react-native'
import { Renderer, useMarkdown, type MarkedStyles } from 'react-native-marked'
import { useCSSVariable, useUniwind } from 'uniwind'

/** A bot's reply, styled with the type roles and the shared colour tokens. */
export function Markdown({ text }: { text: string }) {
  const { theme } = useUniwind()
  const [foreground, muted, border, mutedForeground] = useCSSVariable(['--foreground', '--muted', '--border', '--muted-foreground']) as string[]
  const [bodySize, bodyLine, headingSize, headingLine, metaSize, metaLine] = useCSSVariable(['--text-body', '--text-body--line-height', '--text-heading', '--text-heading--line-height', '--text-meta', '--text-meta--line-height']) as number[]
  const styles = useMemo<MarkedStyles>(() => {
    const body = { fontFamily: 'Geist', fontSize: bodySize, lineHeight: bodyLine, color: foreground }
    // Every level is one heading, as on the web; the library's own sizes, margins and h1/h2 rules are reset. Blocks
    // sit 12 apart (paragraphs pad 6 each side) and a heading takes 4 more above.
    const heading = { ...body, fontSize: headingSize, lineHeight: headingLine, fontWeight: '600' as const, marginVertical: 0, paddingTop: 10, paddingBottom: 6, borderBottomWidth: 0 }
    return {
      text: body, paragraph: { marginVertical: 0, paddingVertical: 6 }, li: { ...body, fontVariant: ['tabular-nums'] }, strong: { ...body, fontWeight: '600' },
      h1: heading, h2: heading, h3: heading, h4: heading,
      link: { ...body, textDecorationLine: 'underline' },
      // 0.9em on foreground at 8% (hex alpha 14); the theme's foreground is an opaque #rrggbb.
      codespan: { fontFamily: 'Geist Mono', fontSize: bodySize * 0.9, fontStyle: 'normal', fontWeight: '400', backgroundColor: `${foreground}14`, color: foreground },
      code: { padding: 12 },
      codeText: { fontFamily: 'Geist Mono', fontSize: metaSize, lineHeight: metaLine, color: foreground },
      blockquote: { borderLeftColor: border, borderLeftWidth: 3, paddingLeft: 12 },
      hr: { backgroundColor: border },
      table: { borderColor: border }, tableCell: { borderColor: border },
      em: { ...body, fontStyle: 'italic', color: mutedForeground },
    }
  }, [foreground, muted, border, mutedForeground, bodySize, bodyLine, headingSize, headingLine, metaSize, metaLine])
  const renderer = useMemo(() => new Reply({ backgroundColor: muted, borderColor: border, borderWidth: 1, borderRadius: 14, marginVertical: 6, overflow: 'hidden' }), [muted, border])
  // A new theme object would re-parse every reply on screen each time the chat renders.
  const colors = useMemo(() => ({ colors: { text: foreground, link: foreground, border, code: muted } }), [foreground, border, muted])
  const nodes = useMarkdown(text, {
    renderer,
    colorScheme: theme === 'dark' ? 'dark' : 'light',
    theme: colors,
    styles,
  })
  return <View className="min-w-0">{nodes}</View>
}

/** Caps Dynamic Type at 2x, as the app's `Text` does; a block's nested runs inherit it from the block's outer text. */
const capped = (node: ReactNode) => isValidElement(node) ? cloneElement(node as ReactElement<TextProps>, { maxFontSizeMultiplier: 2 }) : node

/** The library frames the scrolled content, so a long line ran off the frame; this frames the visible window instead. */
class Reply extends Renderer {
  constructor(private frame: ViewStyle) { super() }
  code(text: string, _language?: string, containerStyle?: ViewStyle, textStyle?: TextStyle) {
    return <View key={this.getKey()} style={this.frame}>
      <ScrollView horizontal contentContainerStyle={containerStyle}><View>{this.text(text, textStyle)}</View></ScrollView>
    </View>
  }
  text(...args: Parameters<Renderer['text']>) { return capped(super.text(...args)) }
  heading(...args: Parameters<Renderer['heading']>) { return capped(super.heading(...args)) }
}
