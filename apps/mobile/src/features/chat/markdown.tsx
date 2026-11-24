import { cloneElement, isValidElement, memo, useMemo, type ReactElement, type ReactNode } from 'react'
import { ScrollView, View, type TextProps, type TextStyle, type ViewStyle } from 'react-native'
import { Renderer, useMarkdown, type MarkedStyles, type useMarkdownHookOptions } from 'react-native-marked'
import { useUniwind } from 'uniwind'
import { arcColors, arcTokens } from '@vibeface/theme/tokens'
import { Button } from '@/components/arc/button'
import { Text } from '@/components/arc/text'
import { useClipboardAction } from '@/lib/use-clipboard'

/** A bot's reply, styled with the type roles and the shared colour tokens. */
export function Markdown({ text }: { text: string }) {
  const { theme } = useUniwind()
  const palette = theme === 'dark' ? arcColors.dark : arcColors.light
  const { foreground, border, 'surface-muted': muted, 'text-secondary': mutedForeground } = palette
  const bodySize = arcTokens.fontSize.base, bodyLine = 24
  const headingSize = arcTokens.fontSize.base, headingLine = 24
  const metaSize = arcTokens.fontSize.sm, metaLine = 20
  const styles = useMemo<MarkedStyles>(() => {
    const body = { fontFamily: 'Inter', fontSize: bodySize, lineHeight: bodyLine, color: foreground }
    // Every level is one heading, as on the web; the library's own sizes, margins and h1/h2 rules are reset. Blocks
    // sit 12 apart (paragraphs pad 6 each side) and a heading takes 4 more above.
    const heading = { ...body, fontSize: headingSize, lineHeight: headingLine, fontWeight: '500' as const, marginVertical: 0, paddingTop: 10, paddingBottom: 6, borderBottomWidth: 0 }
    return {
      text: body, paragraph: { marginVertical: 0, paddingVertical: 6 }, li: { ...body, fontVariant: ['tabular-nums'] }, strong: { ...body, fontWeight: '500' },
      h1: heading, h2: heading, h3: heading, h4: heading,
      link: { ...body, textDecorationLine: 'underline' },
      codespan: { fontFamily: 'Geist Mono', fontSize: metaSize, fontStyle: 'normal', fontWeight: '400', backgroundColor: muted, color: foreground },
      code: { padding: 12 },
      codeText: { fontFamily: 'Geist Mono', fontSize: metaSize, lineHeight: metaLine, color: foreground },
      blockquote: { borderLeftColor: border, borderLeftWidth: 3, paddingLeft: 12 },
      hr: { backgroundColor: border },
      table: { borderColor: border }, tableCell: { borderColor: border },
      em: { ...body, fontStyle: 'italic', color: mutedForeground },
    }
  }, [foreground, muted, border, mutedForeground, bodySize, bodyLine, headingSize, headingLine, metaSize, metaLine])
  const renderer = useMemo(() => new Reply({ backgroundColor: muted, borderColor: border, borderWidth: 1, borderRadius: arcTokens.radius.control, marginVertical: 6, overflow: 'hidden' }), [muted, border])
  // A new theme object would re-parse every reply on screen each time the chat renders.
  const colors = useMemo(() => ({ colors: { text: foreground, link: foreground, border, code: muted } }), [foreground, border, muted])
  const options = useMemo<useMarkdownHookOptions>(() => ({ renderer, colorScheme: theme === 'dark' ? 'dark' : 'light', theme: colors, styles }), [renderer, theme, colors, styles])
  const blocks = useMemo(() => markdownBlocks(text), [text])
  return <View className="min-w-0">{blocks.map((block, index) => <Block key={index} text={block} options={options} />)}</View>
}

/** A finished block keeps its parsed nodes while a streamed reply grows below it; only the block that changed is parsed again. */
const Block = memo(function Block({ text, options }: { text: string; options: useMarkdownHookOptions }) {
  return <>{useMarkdown(text, options)}</>
})

const fenceLine = /^ {0,3}(`{3,}|~{3,})/
/**
 * Cuts the text at blank lines outside code fences, before a line that can only start a new top-level block: not
 * indented, not a list item, quote, table row or HTML, so lists, quotes and code read as they would whole. Reference
 * links and HTML blocks that may span blank lines resolve across the whole text, so such text stays one block.
 */
function markdownBlocks(text: string): string[] {
  if (/^ {0,3}(\[[^\]\n]+\]:|<(!--|pre|script|style|textarea))/im.test(text)) return [text]
  const blocks: string[] = []
  let start = 0, fence = '', blank = false, filled = false
  for (let at = 0; at < text.length;) {
    const end = text.indexOf('\n', at), next = end < 0 ? text.length : end + 1
    const line = text.slice(at, end < 0 ? text.length : end)
    const marker = fenceLine.exec(line)?.[1], info = marker ? line.trim().slice(marker.length) : ''
    if (fence) {
      if (marker && marker[0] === fence[0] && marker.length >= fence.length && !info.trim()) fence = ''
    } else if (!line.trim()) blank = true
    else {
      if (blank && filled && !/^[\s>|<[\-*+\d]/.test(line)) { blocks.push(text.slice(start, at)); start = at }
      blank = false
      filled = true
      // A backtick in a backtick fence's info string makes the line inline code instead.
      if (marker && !(marker[0] === '`' && info.includes('`'))) fence = marker
    }
    at = next
  }
  blocks.push(text.slice(start))
  return blocks
}

/** Caps Dynamic Type at 2x, as the app's `Text` does; a block's nested runs inherit it from the block's outer text. */
const capped = (node: ReactNode) => isValidElement(node) ? cloneElement(node as ReactElement<TextProps>, { maxFontSizeMultiplier: 2 }) : node

/** The library frames the scrolled content, so a long line ran off the frame; this frames the visible window instead. */
class Reply extends Renderer {
  constructor(private frame: ViewStyle) { super() }
  code(text: string, _language?: string, containerStyle?: ViewStyle, textStyle?: TextStyle) {
    return <CodeBlock key={this.getKey()} text={text} frame={this.frame} containerStyle={containerStyle}>{this.text(text, textStyle)}</CodeBlock>
  }
  text(...args: Parameters<Renderer['text']>) { return capped(super.text(...args)) }
  heading(...args: Parameters<Renderer['heading']>) { return capped(super.heading(...args)) }
}

function CodeBlock({ text, frame, containerStyle, children }: { text: string; frame: ViewStyle; containerStyle?: ViewStyle; children: ReactNode }) {
  const copy = useClipboardAction(text)
  return <View style={frame}>
    <View className="flex-row justify-end px-1"><Button variant="ghost" size="sm" disabled={copy.busy} accessibilityLabel="复制代码块" onPress={() => { void copy.copy(text) }}><Text>{copy.copied ? '已复制' : copy.busy ? '复制中' : '复制代码'}</Text></Button></View>
    <ScrollView horizontal contentContainerStyle={containerStyle}><View>{children}</View></ScrollView>
    {copy.error ? <Text accessibilityLiveRegion="polite" className="px-3 pb-3 text-sm text-danger">{copy.error}</Text> : null}
  </View>
}
