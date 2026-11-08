import { Menu, Tray, app, nativeImage } from 'electron'
import { PRODUCT_NAME } from '../shared/contract.js'
import colour from '../../assets/tray.png'
import colour2x from '../../assets/tray@2x.png'
import template from '../../assets/trayTemplate.png'
import template2x from '../../assets/trayTemplate@2x.png'

export type TrayActions = {
  showLauncher(): void
  openBusiness(): void
  quit(): void
}

/**
 * The mark is compiled into the main bundle so a packaged app needs no loose asset file.
 * macOS draws a template image, which follows menu bar appearance; Windows and Linux draw
 * the full-colour mark.
 */
function trayIcon(): Electron.NativeImage {
  const [base, retina] = process.platform === 'darwin' ? [template, template2x] : [colour, colour2x]
  const image = nativeImage.createFromDataURL(base)
  image.addRepresentation({ scaleFactor: 2, dataURL: retina })
  if (process.platform === 'darwin') image.setTemplateImage(true)
  return image
}

export function createTray(actions: TrayActions): Tray {
  const tray = new Tray(trayIcon())
  tray.setToolTip(`${PRODUCT_NAME} ${app.getVersion()}`)
  tray.setContextMenu(
    Menu.buildFromTemplate([
      { label: '启动器…', click: actions.showLauncher },
      { label: `打开 ${PRODUCT_NAME} 窗口`, click: actions.openBusiness },
      { type: 'separator' },
      { label: `退出 ${PRODUCT_NAME}`, click: actions.quit },
    ])
  )
  tray.on('click', actions.showLauncher)
  return tray
}
