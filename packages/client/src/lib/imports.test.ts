import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import path from 'node:path'
import test from 'node:test'
import { fileURLToPath } from 'node:url'
import ts from 'typescript'

test('shared client modules have no static runtime import cycles', () => {
  const root = fileURLToPath(new URL('../../', import.meta.url))
  const config = ts.readConfigFile(path.join(root, 'tsconfig.json'), ts.sys.readFile)
  assert.equal(config.error, undefined)
  const { fileNames, options } = ts.parseJsonConfigFileContent(config.config, ts.sys, root)
  const modules = new Set(fileNames.filter(file => !/\.(?:test|d)\.tsx?$/.test(file) && /\.tsx?$/.test(file)))
  const done = new Set<string>()
  const visiting = new Set<string>()
  function visit(file: string) {
    if (done.has(file)) return
    assert.ok(!visiting.has(file), [...visiting, file].map(f => path.relative(root, f)).join(' -> '))
    visiting.add(file)
    const source = ts.createSourceFile(file, readFileSync(file, 'utf8'), ts.ScriptTarget.Latest)
    for (const node of source.statements) {
      if (!(ts.isImportDeclaration(node) && !node.importClause?.isTypeOnly)
        && !(ts.isExportDeclaration(node) && !node.isTypeOnly)) continue
      const specifier = node.moduleSpecifier
      if (!specifier || !ts.isStringLiteral(specifier)) continue
      const resolved = ts.resolveModuleName(specifier.text, file, options, ts.sys).resolvedModule?.resolvedFileName
      if (resolved && modules.has(resolved)) visit(resolved)
    }
    visiting.delete(file)
    done.add(file)
  }
  assert.ok(modules.size > 0)
  for (const file of modules) visit(file)
})
