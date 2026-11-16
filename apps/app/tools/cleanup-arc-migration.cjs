const fs = require('node:fs')
const path = require('node:path')
const ts = require('../node_modules/typescript')

const project = path.resolve(__dirname, '..')
const config = ts.readConfigFile(path.join(project, 'tsconfig.json'), ts.sys.readFile)
const parsed = ts.parseJsonConfigFileContent(config.config, ts.sys, project)
const program = ts.createProgram(parsed.fileNames, parsed.options)
const checker = program.getTypeChecker()
const write = process.argv.includes('--write')
const excluded = new Set(['src/components/app/bot-face.tsx', 'src/components/arc/conversation-composer.tsx'])

function primitive(node, flags) {
  const type = checker.getTypeAtLocation(node)
  const parts = type.isUnion() ? type.types : [type]
  return parts.every(part => Boolean(part.flags & flags))
}

function literalString(node) {
  if (ts.isStringLiteral(node)) return node.text
  if (ts.isCallExpression(node) && ts.isIdentifier(node.expression) && node.expression.text === 'String' && node.arguments.length === 1) return literalString(node.arguments[0])
  return undefined
}

function unwrap(node) {
  return ts.isParenthesizedExpression(node) ? unwrap(node.expression) : node
}

let changed = 0
for (const source of program.getSourceFiles()) {
  const relative = path.relative(project, source.fileName)
  if (!relative.startsWith('src/') || !relative.endsWith('.tsx') || excluded.has(relative)) continue
  const original = source.text
  const text = node => node.getText(source)
  function rewrite(node) {
    let result = text(node)
    const children = []
    ts.forEachChild(node, child => {
      const next = rewrite(child)
      if (next !== text(child)) {
        let start = child.getStart(source) - node.getStart(source)
        if (!next && ts.isJsxAttribute(child) && result[start - 1] === ' ') start--
        children.push({ start, end: child.end - node.getStart(source), next })
      }
    })
    for (const child of children.sort((a, b) => b.start - a.start)) result = result.slice(0, child.start) + child.next + result.slice(child.end)

    if (ts.isJsxAttribute(node) && node.initializer) {
      if (text(node.name) === 'className' && ts.isStringLiteral(node.initializer) && !node.initializer.text.trim()) return ''
      if (ts.isJsxExpression(node.initializer) && node.initializer.expression && ts.isStringLiteral(node.initializer.expression)) {
        const value = node.initializer.expression.text
        if (text(node.name) === 'className' && !value.trim()) return ''
        if (!/["<>&\r\n\\]/.test(value)) return `${text(node.name)}="${value}"`
      }
    }
    if (ts.isBinaryExpression(node) && node.operatorToken.kind === ts.SyntaxKind.PlusToken) {
      const left = literalString(node.left), right = literalString(node.right)
      if (left !== undefined && right !== undefined) return JSON.stringify(left + right)
    }
    if (ts.isCallExpression(node) && ts.isIdentifier(node.expression) && node.expression.text === 'String' && node.arguments.length === 1 && primitive(node.arguments[0], ts.TypeFlags.StringLike)) {
      const argument = node.arguments[0]
      const value = rewrite(argument)
      return ts.isBinaryExpression(argument) || ts.isConditionalExpression(argument) ? `(${value})` : value
    }
    if (ts.isCallExpression(node) && ts.isPropertyAccessExpression(node.expression) && node.expression.name.text === 'join' && ts.isArrayLiteralExpression(node.expression.expression) && node.arguments.length === 1 && ts.isStringLiteral(node.arguments[0])) {
      const elements = node.expression.expression.elements
      if (elements.length === 1 && primitive(elements[0], ts.TypeFlags.StringLike)) return rewrite(elements[0])
      if (elements.length > 1 && elements.every(element => !ts.isSpreadElement(element) && primitive(element, ts.TypeFlags.StringLike | ts.TypeFlags.NumberLike | ts.TypeFlags.BigIntLike | ts.TypeFlags.BooleanLike))) {
        const parts = elements.map(element => ts.isStringLiteral(element) ? element.text.replace(/\\/g, '\\\\').replace(/`/g, '\\`').replace(/\$\{/g, '\\${') : '${' + rewrite(element) + '}')
        return '`' + parts.join(node.arguments[0].text) + '`'
      }
    }
    // Only generated Arc Select option arrays have this exact null-filter form.
    if (ts.isCallExpression(node) && ts.isPropertyAccessExpression(node.expression) && node.expression.name.text === 'filter' && node.arguments.length === 1 && /^(\w+)\s*=>\s*\1 !== null && \1 !== undefined$/.test(text(node.arguments[0]))) {
      const flat = node.expression.expression
      if (ts.isCallExpression(flat) && !flat.arguments.length && ts.isPropertyAccessExpression(flat.expression) && flat.expression.name.text === 'flat' && ts.isArrayLiteralExpression(flat.expression.expression) && ts.isJsxExpression(node.parent) && ts.isJsxAttribute(node.parent.parent) && text(node.parent.parent.name) === 'options') {
        const options = []
        for (const item of flat.expression.expression.elements) {
          const option = unwrap(item)
          if (ts.isObjectLiteralExpression(option)) options.push(rewrite(option))
          else if (checker.isArrayType(checker.getTypeAtLocation(option))) options.push('...' + rewrite(option))
          else if (ts.isConditionalExpression(option) && option.whenFalse.kind === ts.SyntaxKind.NullKeyword && ts.isObjectLiteralExpression(unwrap(option.whenTrue))) options.push(`...(${rewrite(option.condition)} ? [${rewrite(unwrap(option.whenTrue))}] : [])`)
          else return result
        }
        return options.length === 1 && options[0].startsWith('...') ? options[0].slice(3) : '[' + options.join(', ') + ']'
      }
    }
    return result
  }
  const next = rewrite(source)
  if (next === original) continue
  changed++
  process.stdout.write(`${write ? 'Updated' : 'Would update'} ${relative}\n`)
  if (write) fs.writeFileSync(source.fileName, next)
}
process.stdout.write(`${changed} file(s) ${write ? 'updated' : 'need cleanup'}\n`)
