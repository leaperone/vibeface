import { Link } from '@tanstack/react-router'

export function NotFound() {
  return <main className="site-shell not-found"><p className="eyebrow">404 · 页面不存在</p><h1>这条路径没有内容。</h1><p>返回首页，或从文档目录重新开始。</p><Link className="button button-primary" to="/">回到首页</Link></main>
}
