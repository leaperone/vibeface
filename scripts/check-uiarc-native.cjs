const assert = require('node:assert/strict')
const fs = require('node:fs')
const path = require('node:path')
const Module = require('node:module')

const root = path.resolve(__dirname, '..')
const app = path.join(root, 'apps/mobile')
const nativeRequire = Module.createRequire(path.join(app, 'package.json'))
const transformer = path.join(path.dirname(nativeRequire.resolve('uniwind/package.json')), 'dist/metro/transformer.cjs')
const compiler = new Module(transformer, module)
compiler.filename = transformer
compiler.paths = Module._nodeModulePaths(path.dirname(transformer))
// Uniwind keeps this compiler private. Exercise that exact installed pipeline,
// rather than duplicate its CSS-to-native conversion in the verification code.
compiler._compile(fs.readFileSync(transformer, 'utf8') + '\nexports.verifyCompile = compileCSS; exports.verifyConfig = config.UniwindBundlerConfig;', transformer)

async function main() {
  process.chdir(app)
  const { arcColors, arcTokens } = nativeRequire('@vibeface/theme/tokens')
  const report = {}
  for (const platform of ['ios', 'android']) {
    const config = compiler.exports.verifyConfig.fromMetroConfig({ cssEntryFile: './global.css' }, platform)
    await config.generateArtifacts(path.join(path.dirname(nativeRequire.resolve('uniwind/package.json')), 'uniwind.css'))
    const content = await compiler.exports.verifyCompile(config)
    const compiled = new Function('rt', 'return ' + content)({ colorScheme: 'light' })
    report[platform] = {}
    for (const theme of ['light', 'dark']) {
      const variables = { ...compiled.vars, ...compiled.scopedVars['__uniwind-theme-' + theme] }
      const read = (candidate, property) => {
        const entry = compiled.stylesheet[candidate]?.[0].entries.find(([name]) => name === property)
        assert.ok(entry, `${platform}/${theme}: ${candidate} has no native ${property}`)
        return entry[1](variables)
      }
      const styles = {}
      for (const [candidate, property, token] of [
        ['text-secondary', 'color', 'text-secondary'],
        ['text-muted', 'color', 'text-muted'],
        ['border-strong', 'borderColor', 'border-strong'],
        ['border-subtle', 'borderColor', 'border-subtle'],
        ['bg-surface', 'backgroundColor', 'surface'],
      ]) {
        const actual = read(candidate, property)
        assert.equal(actual, arcColors[theme][token], `${platform}/${theme}: ${candidate}`)
        styles[candidate] = actual
      }
      for (const size of ['sm', 'base', 'lg', 'xl', '2xl']) {
        const candidate = 'text-' + size
        assert.equal(read(candidate, 'fontSize'), arcTokens.fontSize[size])
        assert.ok(Math.abs(read(candidate, 'lineHeight') - arcTokens.fontSize[size] * 1.4) < .0001, `${platform}/${theme}: ${candidate} line height`)
      }
      assert.equal(read('rounded-control', 'borderRadius'), 18)
      assert.equal(read('rounded-panel', 'borderRadius'), 26)
      assert.equal(read('p-5', 'padding'), 20)
      report[platform][theme] = styles
    }
  }
  const output = path.join(root, '.artifacts/uiarc/native-style-check.json')
  fs.mkdirSync(path.dirname(output), { recursive: true })
  fs.writeFileSync(output, JSON.stringify(report, null, 2) + '\n')
  console.log('Arc actual iOS and Android compiled colors, line heights and geometry verified')
}

main().catch((error) => { console.error(error); process.exitCode = 1 })
