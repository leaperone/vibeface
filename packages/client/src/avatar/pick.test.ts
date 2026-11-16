import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import test from 'node:test'

test('pure avatar, settings, model and timeline imports work while React resolution is forbidden', () => {
  const result = spawnSync(process.execPath, ['--experimental-strip-types', '--input-type=module', '--eval', `
    import assert from 'node:assert/strict'
    import { registerHooks } from 'node:module'

    registerHooks({
      resolve(specifier, context, nextResolve) {
        if (['react', 'react-dom', 'react-native'].some(pkg => specifier === pkg || specifier.startsWith(pkg + '/'))) {
          throw new Error('React forbidden: ' + specifier)
        }
        return nextResolve(specifier, context)
      },
    })

    await assert.rejects(import('react'), /React forbidden: react/)
    const [pickPath, settingsPath, modelPath, timelinePath] = process.argv.slice(1)
    const pick = await import(pickPath)
    const settings = await import(settingsPath)
    const model = await import(modelPath)
    const timeline = await import(timelinePath)

    assert.equal(pick.avatarWords({ outline: 'round', eyes: 'dot', hue: 255 }), '圆、圆点眼、蓝色')
    assert.equal(pick.avatarWrite({ avatar: null, avatar_version: null }, null, { outline: 'cloud', eyes: 'wide', hue: 285 }), 'unsent')
    assert.equal('useAvatarPick' in pick, false)
    assert.equal(settings.overwriteCopy.line({ change_id: 'change', by: 'phone', field: 'avatar', value: null }, 'Bot', '手机'), '手机 把 Bot 的长相改成了默认长相')
    assert.deepEqual(model.parseNodes({ nodes: [] }), [])
    assert.deepEqual(timeline.byRank([{ rank: 'b' }, { rank: 'a' }]), [{ rank: 'a' }, { rank: 'b' }])
  `, ...['./pick.ts', '../workspace/bot-settings.ts', '../workspace/model.ts', '../chat/timeline.ts'].map(path => new URL(path, import.meta.url).href)], {
    encoding: 'utf8',
    timeout: 10_000,
  })
  assert.equal(result.status, 0, `${result.error?.message ?? ''}\n${result.stdout}\n${result.stderr}`)
})
