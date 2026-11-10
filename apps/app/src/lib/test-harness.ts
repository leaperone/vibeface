import { readFileSync } from 'node:fs'
import { extname } from 'node:path'
import { Script, type Context } from 'node:vm'
import { JsxEmit, ModuleKind, transpileModule } from 'typescript'

export function moduleLoader(
  sandbox: Context,
  require: (specifier: string, filename: string) => unknown,
  source = (filename: string) => readFileSync(filename, 'utf8'),
) {
  const modules = new Map<string, { exports: Record<string, unknown> }>()
  return function load(filename: string): Record<string, unknown> {
    if (extname(filename) === '.json') {
      return { __esModule: true, default: JSON.parse(readFileSync(filename, 'utf8')) }
    }
    const cached = modules.get(filename)
    if (cached) return cached.exports
    const module = { exports: {} }
    modules.set(filename, module)
    const compiled = transpileModule(source(filename), {
      compilerOptions: { module: ModuleKind.CommonJS, jsx: JsxEmit.ReactJSX },
    }).outputText
    new Script(`(function (exports, require, module) {\n${compiled}\n})`, { filename }).runInContext(sandbox)(
      module.exports,
      (specifier: string) => require(specifier, filename),
      module,
    )
    return module.exports
  }
}

export function hookHarness(lazyInitialState = false) {
  const values: unknown[] = []
  const effects = new Map<number, { dependencies: unknown[]; cleanup?: () => void }>()
  const afterRender: (() => void)[] = []
  let cursor = 0
  const react = {
    createContext: () => ({}),
    useContext: () => null,
    useState(initial: unknown) {
      const index = cursor++
      if (!(index in values)) {
        values[index] = lazyInitialState && typeof initial === 'function' ? initial() : initial
      }
      return [values[index], (next: unknown) => {
        values[index] = typeof next === 'function' ? next(values[index]) : next
      }]
    },
    useRef(initial: unknown) {
      const index = cursor++
      if (!(index in values)) values[index] = { current: initial }
      return values[index]
    },
    useEffect(run: () => (() => void) | void, dependencies: unknown[]) {
      const index = cursor++
      const previous = effects.get(index)
      if (!previous || dependencies.some((value, i) => value !== previous.dependencies[i])) {
        afterRender.push(() => {
          previous?.cleanup?.()
          effects.set(index, { dependencies, cleanup: run() ?? undefined })
        })
      }
    },
  }
  return {
    react, values, effects, afterRender,
    next: () => cursor++,
    reset() {
      cursor = 0
    },
  }
}
