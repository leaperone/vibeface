import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import { createRequire } from 'node:module'

const ts = createRequire(new URL('../packages/client/package.json', import.meta.url))('typescript')

for (const project of ['packages/client', 'apps/app']) {
  const root = fileURLToPath(new URL(`../${project}/`, import.meta.url))
  const config = ts.readConfigFile(path.join(root, 'tsconfig.json'), ts.sys.readFile)
  assert.equal(config.error, undefined)
  const { fileNames, options, errors } = ts.parseJsonConfigFileContent(config.config, ts.sys, root)
  assert.deepEqual(errors, [])
  assert.equal(options.verbatimModuleSyntax, true, 'runtime edges require verbatim imports')
  const modules = new Set(fileNames.filter(file => !/\.(?:test|d)\.tsx?$/.test(file) && /\.tsx?$/.test(file)))
  const done = new Set()
  const visiting = new Set()
  function visit(file) {
    if (done.has(file)) return
    assert.ok(!visiting.has(file), [...visiting, file].map(f => path.join(project, path.relative(root, f))).join(' -> '))
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
  console.log(`${project}: ${modules.size} modules, no static runtime import cycles`)
}
