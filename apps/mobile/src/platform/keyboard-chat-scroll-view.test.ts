import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { createContext, Script } from 'node:vm'
import { createRequire } from 'node:module'
import { resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import test from 'node:test'

function loadFabricPadding() {
  const packageRoot = fileURLToPath(new URL('../../node_modules/react-native-keyboard-controller/', import.meta.url))
  const packageJson: unknown = JSON.parse(readFileSync(resolve(packageRoot, 'package.json'), 'utf8'))
  assert.ok(packageJson && typeof packageJson === 'object' && 'version' in packageJson)
  assert.equal(packageJson.version, '1.22.7', 'private renderer assertions are pinned to the reviewed keyboard-controller release')
  const root = resolve(packageRoot, 'lib/commonjs/components/KeyboardChatScrollView')
  const modules = new Map<string, { exports: Record<string, unknown> }>()
  const context = createContext({})
  const load = (sourceFile: string): Record<string, unknown> => {
    const existing = modules.get(sourceFile)
    if (existing) return existing.exports
    const module = { exports: {} }
    modules.set(sourceFile, module)
    const source = readFileSync(sourceFile, 'utf8')
    const localRequire = (specifier: string) => {
      if (specifier === 'react') return { useCallback: (callback: unknown) => callback }
      if (specifier === 'react-native') return { Platform: { OS: 'ios' } }
      if (specifier === 'react-native-reanimated') return { interpolate: (value: number) => value }
      if (specifier === '../../../architecture') return { IS_FABRIC: true }
      if (specifier === '../../../reanimated') return { useAnimatedReaction: animatedReaction }
      if (specifier === '../../utils/safeScrollTo') return { safeScrollTo: () => assert.fail('iOS Fabric must update the inset and offset together') }
      if (specifier.startsWith('.')) return load(createRequire(sourceFile).resolve(specifier))
      throw new Error(`Unexpected keyboard-controller dependency ${specifier}`)
    }
    new Script(`(function (exports, require, module) {\n${source}\n})`, { filename: sourceFile })
      .runInContext(context)(
        module.exports,
        localRequire,
        module,
      )
    return module.exports
  }
  let reactionRead: (() => number) | undefined
  let reactionRun: ((current: number, previous: number | null) => void) | undefined
  let previous: number | null = null
  const animatedReaction = (read: () => number, react: (current: number, previous: number | null) => void) => {
    reactionRead = read
    reactionRun = react
    react(read(), previous)
    previous = read()
  }
  const hook = load(resolve(root, 'useExtraContentPadding/index.js')).useExtraContentPadding
  if (typeof hook !== 'function') throw new Error('keyboard-controller private hook is not callable')
  return {
    useExtraContentPadding(options: Record<string, unknown>) { Reflect.apply(hook, undefined, [options]) },
    runReaction() {
      assert.ok(reactionRead && reactionRun)
      const current = reactionRead()
      reactionRun(current, previous)
      previous = current
    },
  }
}

test('Fabric extra padding keeps the latest edge during dock height changes', () => {
  const source = loadFabricPadding()

  const scrollOffset = { value: 0 }
  const contentOffsetY = { value: 0 }
  const keyboardPadding = { value: 0 }
  const dockHeight = { value: 0 }
  const options = {
    scrollViewRef: {}, extraContentPadding: dockHeight, keyboardPadding,
    blankSpace: { value: 0 }, scroll: scrollOffset, contentOffsetY,
    layout: { value: { height: 844 } }, size: { value: { height: 2000 } },
    inverted: true, keyboardLiftBehavior: 'whenAtEnd', freeze: { value: false },
  }

  source.useExtraContentPadding(options)
  dockHeight.value = 94
  source.runReaction()
  assert.equal(contentOffsetY.value, -94, 'the installed hook reserves the complete dock on its first measurement')
  scrollOffset.value = contentOffsetY.value
  dockHeight.value = 94
  source.runReaction()
  assert.equal(contentOffsetY.value, -94, 'the same measurement must not add a second inset correction')
  dockHeight.value = 190
  source.runReaction()
  assert.equal(contentOffsetY.value, -190, 'dock growth preserves the latest edge including the initial inset')
  scrollOffset.value = contentOffsetY.value
  dockHeight.value = 94
  source.runReaction()
  assert.equal(contentOffsetY.value, -94, 'dock shrink preserves the latest edge')
  scrollOffset.value = 400
  keyboardPadding.value = 302
  dockHeight.value = 190
  source.runReaction()
  assert.equal(contentOffsetY.value, -94, 'the installed hook issues no offset change while reading older messages')
})
