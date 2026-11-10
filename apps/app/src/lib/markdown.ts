import MarkdownIt from "markdown-it"

// html: false escapes raw HTML; validateLink drops javascript:, vbscript:, file: and data: links. Images are off so replies cannot load remote trackers.
const md = new MarkdownIt({ html: false, linkify: false, typographer: false, breaks: true }).disable("image")
md.renderer.rules.link_open = (tokens, index, options, _env, self) => {
  tokens[index].attrSet("target", "_blank")
  tokens[index].attrSet("rel", "noopener noreferrer")
  return self.renderToken(tokens, index, options)
}
const FOLD_LINES = 16
const chevron = '<svg aria-hidden="true" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m6 9 6 6 6-6"/></svg>'
md.renderer.rules.fence = (tokens, index) => {
  const token = tokens[index]
  const lang = md.utils.escapeHtml(token.info.trim().split(/\s+/)[0] ?? "")
  const fold = token.content.replace(/\n$/, "").split("\n").length > FOLD_LINES
    ? `<button type="button" class="md-fold" data-fold aria-expanded="false">${chevron}<span class="md-fold-more">展开</span><span class="md-fold-less">收起</span></button>`
    : ""
  return `<div class="md-code"><div class="md-code-head"><span>${lang}</span><button type="button" class="hit-area relative" data-copy aria-label="复制">复制</button></div><pre><code>${md.utils.escapeHtml(token.content)}</code></pre>${fold}</div>`
}
md.renderer.rules.table_open = () => '<div class="md-table"><table>'
md.renderer.rules.table_close = () => "</table></div>"

export function renderMarkdown(text: string) {
  return md.render(text)
}
