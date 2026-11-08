import { Menu, app, type MenuItemConstructorOptions } from 'electron'
import { PRODUCT_NAME } from '../shared/contract.js'
import { openExternally } from './windows.js'

const RELEASES = 'https://github.com/leaperone/vibeface/releases'
const DOCS = 'https://github.com/leaperone/vibeface/tree/main/packages/docs'

export type MenuActions = {
  openNodeSettings(): void
  focusClient(): void
  quit(): void
}

export function applyApplicationMenu(actions: MenuActions): void {
  const appMenu: MenuItemConstructorOptions = {
    label: PRODUCT_NAME,
    submenu: [
      { role: 'about', label: `关于 ${PRODUCT_NAME}` },
      { type: 'separator' },
      { label: '本机节点…', accelerator: 'CmdOrCtrl+,', click: actions.openNodeSettings },
      { label: `显示 ${PRODUCT_NAME}`, accelerator: 'CmdOrCtrl+Shift+O', click: actions.focusClient },
      { type: 'separator' },
      ...(process.platform === 'darwin'
        ? ([{ role: 'hide' }, { role: 'hideOthers' }, { role: 'unhide' }, { type: 'separator' }] satisfies MenuItemConstructorOptions[])
        : []),
      { label: `退出 ${PRODUCT_NAME}`, accelerator: process.platform === 'darwin' ? 'Cmd+Q' : 'Alt+F4', click: actions.quit },
    ],
  }

  const template: MenuItemConstructorOptions[] = [
    appMenu,
    {
      label: '编辑',
      submenu: [
        { role: 'undo', label: '撤销' },
        { role: 'redo', label: '重做' },
        { type: 'separator' },
        { role: 'cut', label: '剪切' },
        { role: 'copy', label: '复制' },
        { role: 'paste', label: '粘贴' },
        { role: 'selectAll', label: '全选' },
      ],
    },
    {
      label: '视图',
      submenu: [
        { role: 'reload', label: '重新载入' },
        { role: 'resetZoom', label: '实际大小' },
        { role: 'zoomIn', label: '放大' },
        { role: 'zoomOut', label: '缩小' },
        { type: 'separator' },
        { role: 'togglefullscreen', label: '全屏' },
      ],
    },
    {
      label: '窗口',
      submenu: [
        { role: 'minimize', label: '最小化' },
        { role: 'close', label: '关闭窗口' },
        ...(process.platform === 'darwin'
          ? ([{ type: 'separator' }, { role: 'front', label: '前置全部窗口' }] satisfies MenuItemConstructorOptions[])
          : []),
      ],
    },
    {
      role: 'help',
      label: '帮助',
      submenu: [
        { label: '使用文档', click: () => openExternally(DOCS) },
        { label: '发行版本', click: () => openExternally(RELEASES) },
        { type: 'separator' },
        { label: `版本 ${app.getVersion()}`, enabled: false },
      ],
    },
  ]

  Menu.setApplicationMenu(Menu.buildFromTemplate(template))
}
