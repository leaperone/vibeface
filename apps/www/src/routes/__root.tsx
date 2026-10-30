import { HeadContent, Outlet, Scripts, createRootRoute } from '@tanstack/react-router'
import { RootProvider } from 'fumadocs-ui/provider/tanstack'
import { StaticSearchDialog } from '@/components/static-search-dialog'
import '@fontsource-variable/geist'
import '@fontsource-variable/geist-mono'
import '../styles/app.css'

export const Route = createRootRoute({
  head: () => ({
    meta: [
      { charSet: 'utf-8' },
      { name: 'viewport', content: 'width=device-width, initial-scale=1' },
      { title: 'Vibeface · 个人设备上的 Agent 网络' },
      {
        name: 'description',
        content: '在自己的 Mac 或 Linux 设备上运行 Vibeface Agent，连接工作空间、Provider 与个人数据。',
      },
      { name: 'theme-color', content: '#ffffff' },
    ],
    links: [{ rel: 'icon', href: '/logo.svg', type: 'image/svg+xml' }],
  }),
  component: RootComponent,
})

function RootComponent() {
  return (
    <html lang="zh-CN" suppressHydrationWarning>
      <head><HeadContent /></head>
      <body>
        <RootProvider
          search={{ SearchDialog: StaticSearchDialog }}
          i18n={{
            locale: 'zh-CN',
            defaultLanguage: 'zh-CN',
            translations: {
              'Search(search dialog)': '搜索文档',
              'Search(search trigger)': '搜索文档',
              'Open Search(search trigger)(aria-label)': '打开文档搜索',
              'Close Search(search dialog)(aria-label)': '关闭搜索',
              'No results found(search dialog)': '没有找到结果',
              'Search failed(search dialog)': '搜索失败',
              'On this page(table of contents)': '本页目录',
              'No Headings(table of contents)': '没有标题',
              'Copy Anchor Link(heading anchor)(aria-label)': '复制锚点链接',
              'Copied Anchor Link(heading anchor)(aria-label)': '已复制锚点链接',
              'Toggle Theme(theme switcher)(aria-label)': '切换主题',
              'Light(theme switcher)(aria-label)': '浅色主题',
              'Dark(theme switcher)(aria-label)': '深色主题',
              'System(theme switcher)(aria-label)': '跟随系统',
            },
          }}
        ><Outlet /></RootProvider>
        <Scripts />
      </body>
    </html>
  )
}
