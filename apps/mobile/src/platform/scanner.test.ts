import assert from 'node:assert/strict'
import test from 'node:test'
import { fileURLToPath } from 'node:url'
import { createContext } from 'node:vm'
import { Children, isValidElement, type ReactElement, type ReactNode } from 'react'
import * as jsxRuntime from 'react/jsx-runtime'
import { hookHarness, moduleLoader } from '../../../app/src/lib/test-harness.ts'

type Permission = { granted: boolean; canAskAgain: boolean }
type NodeProps = {
  children?: ReactNode
  error?: string
  onPress?: () => void
  onMountError?: () => void
  onBarcodeScanned?: (event: { data: string }) => void
}

function fixture(initial: Permission) {
  const hooks = hookHarness()
  const seen: string[] = []
  let permission: Permission | null = null, systemPermission = initial
  let checkError = false
  let settings = 0, checks = 0, requests = 0, exits = 0
  let onAppState: ((state: string) => void) | undefined
  const getPermission = async () => {
    checks++
    if (checkError) throw new Error('native permission read failure')
    return permission = systemPermission
  }
  const requestPermission = async () => {
    requests++
    return permission = systemPermission
  }
  const load = moduleLoader(createContext({}), (name) => {
    switch (name) {
      case 'react': return hooks.react
      case 'react/jsx-runtime': return jsxRuntime
      case 'expo-camera': return { CameraView: 'CameraView', useCameraPermissions: () => [permission, requestPermission, getPermission] }
      case 'lucide-react-native': return { Camera: 'Camera' }
      case 'react-native': return {
        View: 'View',
        AppState: { addEventListener(_event: string, listener: (state: string) => void) {
          onAppState = listener
          return { remove() { onAppState = undefined } }
        } },
        Linking: { async openSettings() { settings++ } },
      }
      case '@/components/action': return { ErrorText: 'ErrorText', useAction: () => ({ busy: null, error: '', run: (_key: string, action: () => unknown) => action() }) }
      case '@/components/kit': return { Panel: 'Panel' }
      case '@/components/arc/button': return { Button: 'Button' }
      case '@/components/arc/text': return { Text: 'Text' }
      default: throw new Error(`Unexpected import ${name}`)
    }
  })
  const { Scanner } = load(fileURLToPath(new URL('../features/devices/scanner.tsx', import.meta.url)))
  assert(typeof Scanner === 'function')
  const scan = Scanner
  function render() {
    hooks.reset()
    const result: unknown = Reflect.apply(scan, undefined, [{ title: '扫码', onCancel: () => { exits++ }, onScanned: (data: string) => { seen.push(data) } }])
    for (const runEffect of hooks.afterRender.splice(0)) runEffect()
    assert(isValidElement<NodeProps>(result))
    const nodes = elements(result)
    return {
      node: (kind: string) => nodes.find((node) => node.type === kind)?.props,
      button: (label: string) => nodes.find((node) => node.type === 'Button' && elements(node).some((child) => child.type === 'Text' && child.props.children === label))?.props.onPress,
    }
  }
  return {
    render, seen, settings: () => settings, checks: () => checks, requests: () => requests, exits: () => exits,
    setPermission(next: Permission) { systemPermission = next },
    failCheck(fail: boolean) { checkError = fail },
    appState(state: string) { assert(onAppState); onAppState(state) },
  }
}

function elements(value: ReactNode): ReactElement<NodeProps>[] {
  return Children.toArray(value).flatMap((child) => isValidElement<NodeProps>(child) ? [child, ...elements(child.props.children)] : [])
}

test('scanner keeps paste available as permission loads and rechecks after a grant in Settings', async () => {
  const scanner = fixture({ granted: false, canAskAgain: false })
  const paste = scanner.render().button('不扫了，粘贴链接')
  assert(paste)
  paste()
  assert.equal(scanner.exits(), 1)
  await Promise.resolve()

  const blocked = scanner.render()
  assert.equal(blocked.button('允许使用相机'), undefined)
  const settings = blocked.button('打开系统设置')
  assert(settings)
  settings()
  assert.equal(scanner.settings(), 1)
  assert.equal(scanner.requests(), 0)

  scanner.setPermission({ granted: true, canAskAgain: true })
  scanner.appState('background')
  assert.equal(scanner.checks(), 1)
  scanner.appState('active')
  await Promise.resolve()
  assert.equal(scanner.checks(), 2)
  assert(scanner.render().node('CameraView'))
})

test('scanner preserves the exit and a working retry after permission-read and camera-mount failures', async () => {
  const scanner = fixture({ granted: true, canAskAgain: true })
  scanner.failCheck(true)
  scanner.render()
  await Promise.resolve()
  const noPermission = scanner.render()
  assert.equal(noPermission.node('ErrorText')?.error, '无法读取相机权限。')
  assert(noPermission.button('不扫了，粘贴链接'))
  const retryPermission = noPermission.button('重试')
  assert(retryPermission)
  scanner.failCheck(false)
  retryPermission()
  await Promise.resolve()

  const onMountError = scanner.render().node('CameraView')?.onMountError
  assert(onMountError)
  onMountError()
  const mountFailed = scanner.render()
  assert.equal(mountFailed.node('CameraView'), undefined)
  assert.equal(mountFailed.node('ErrorText')?.error, '无法启动相机。')
  assert(mountFailed.button('不扫了，粘贴链接'))
  const retryMount = mountFailed.button('重试')
  assert(retryMount)
  retryMount()
  const onBarcodeScanned = scanner.render().node('CameraView')?.onBarcodeScanned
  assert(onBarcodeScanned)
  onBarcodeScanned({ data: 'sample QR text' })
  assert.deepEqual(scanner.seen, ['sample QR text'])
})
