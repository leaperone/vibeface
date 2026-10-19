import MarkdownIt from "markdown-it"

// html: false escapes raw HTML; validateLink drops javascript:, vbscript:, file: and data: links. Images are off so replies cannot load remote trackers.
const md = new MarkdownIt({ html: false, linkify: false, typographer: false, breaks: true }).disable("image")
md.renderer.rules.link_open = (tokens, index, options, _env, self) => {
  tokens[index].attrSet("target", "_blank")
  tokens[index].attrSet("rel", "noopener noreferrer")
  return self.renderToken(tokens, index, options)
}
md.renderer.rules.fence = (tokens, index) => {
  const token = tokens[index]
  const lang = md.utils.escapeHtml(token.info.trim().split(/\s+/)[0] ?? "")
  return `<div class="md-code"><div class="md-code-head"><span>${lang}</span><button type="button" data-copy aria-label="复制">复制</button></div><pre><code>${md.utils.escapeHtml(token.content)}</code></pre></div>`
}
md.renderer.rules.table_open = () => '<div class="md-table"><table>'
md.renderer.rules.table_close = () => "</table></div>"

export function renderMarkdown(text: string) {
  return md.render(text)
}
