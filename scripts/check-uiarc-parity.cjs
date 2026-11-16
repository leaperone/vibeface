const assert = require('node:assert/strict')
const childProcess = require('node:child_process')
const fs = require('node:fs')
const path = require('node:path')
const { createRequire } = require('node:module')

const root = path.resolve(__dirname, '..')
const appRequire = createRequire(path.join(root, 'apps/app/package.json'))
const ts = appRequire('typescript')
const config = JSON.parse(fs.readFileSync(path.join(root, 'config/uiarc.json'), 'utf8'))
const baseline = process.argv[2] ?? config.baseline
const git = (...args) => childProcess.execFileSync('git', args, { cwd: root, encoding: 'utf8' })
const files = git('ls-tree', '-r', '--name-only', baseline, 'apps/mobile/app', 'apps/mobile/src').trim().split('\n')
  .filter(file => file.endsWith('.tsx') && !file.includes('/components/ui/'))

function inventory(source, file) {
  const tree = ts.createSourceFile(file, source, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX)
  const copy = new Map()
  const events = new Map()
  const add = (map, value) => map.set(value, (map.get(value) ?? 0) + 1)
  const visit = node => {
    if (ts.isStringLiteralLike(node) && /[\u3400-\u9fff]/u.test(node.text)) add(copy, node.text)
    if (ts.isJsxText(node) && /[\u3400-\u9fff]/u.test(node.text)) add(copy, node.text.replace(/\s+/g, ' ').trim())
    if (ts.isJsxAttribute(node) && /^on[A-Z]/.test(node.name.getText(tree)) && node.initializer) {
      add(events, node.name.getText(tree) + '=' + node.initializer.getText(tree).replace(/\s+/g, ''))
    }
    ts.forEachChild(node, visit)
  }
  visit(tree)
  return { copy, events }
}

const report = { baseline, files: files.length, copyInstances: 0, eventBindings: 0, missingCopy: [], missingEvents: [] }
for (const file of files) {
  const previous = inventory(git('show', baseline + ':' + file), file)
  const currentPath = path.join(root, file)
  let current = fs.existsSync(currentPath) ? fs.readFileSync(currentPath, 'utf8') : ''
  if (file === 'apps/mobile/src/features/home/tasks.tsx') {
    current += '\n' + fs.readFileSync(path.join(root, 'apps/mobile/src/components/arc/metric.tsx'), 'utf8')
  }
  const next = inventory(current, file)
  for (const [value, count] of previous.copy) {
    report.copyInstances += count
    if (count > (next.copy.get(value) ?? 0)) report.missingCopy.push({ file, value, missing: count - (next.copy.get(value) ?? 0) })
  }
  for (const [value, count] of previous.events) {
    report.eventBindings += count
    if (count > (next.events.get(value) ?? 0)) report.missingEvents.push({ file, value, missing: count - (next.events.get(value) ?? 0) })
  }
}
const output = path.join(root, '.artifacts/uiarc/native-parity.json')
fs.mkdirSync(path.dirname(output), { recursive: true })
fs.writeFileSync(output, JSON.stringify(report, null, 2) + '\n')
assert.equal(report.missingCopy.length, 0, 'Native product copy is missing; inspect ' + output)
assert.equal(report.missingEvents.length, 0, 'Native product event expressions are missing; inspect ' + output)
console.log(`Native static parity: ${report.copyInstances} copy instances and ${report.eventBindings} event bindings retained across ${report.files} original modules`)
console.log('This compares source expressions; it does not prove component callbacks execute on a device.')
