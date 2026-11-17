/** A returned Promise settles physical cleanup, not merely the request to retire. */
export type DisposableSelection = { dispose?(): void | Promise<void> }

/** UI owns the selected objects; each operation borrows a synchronous snapshot until it settles. */
export function selectionOwner<File extends DisposableSelection>(onCleanupError: (error: AggregateError) => void) {
  let selected = new Set<File>()
  let closed = false
  const uses = new Map<File, number>()
  const retired = new Set<File>()
  const released = new WeakSet<File>()
  const disposing = new WeakSet<File>()
  const pending = new WeakSet<File>()
  const complete = (file: File) => { retired.delete(file); released.add(file) }
  const report = (errors: unknown[]) => onCleanupError(new AggregateError(errors, '未能清理全部选择副本；失败副本保留待重试。'))
  // Commands commit selection metadata synchronously. Physical cleanup can finish later;
  // the required error sink also observes failures after the operation/UI has gone away.
  const cleanup = () => {
    const errors: unknown[] = []
    for (const file of retired) {
      if (uses.has(file) || pending.has(file)) continue
      try {
        disposing.add(file)
        const completion = file.dispose?.()
        if (completion) {
          pending.add(file)
          void completion.then(() => { pending.delete(file); complete(file) }, cause => {
            pending.delete(file)
            report([cause])
          })
        } else complete(file)
      } catch (cause) { errors.push(cause) }
    }
    if (errors.length) report(errors)
  }
  return {
    replace(files: readonly File[]) {
      const next = new Set(files)
      if (closed) {
        for (const file of next) if (!released.has(file)) retired.add(file)
        cleanup()
        return false
      }
      if ([...next].some(file => disposing.has(file))) throw new Error('请重新选择文件。')
      for (const file of selected) if (!next.has(file)) retired.add(file)
      for (const file of next) retired.delete(file)
      selected = next
      cleanup()
      return true
    },
    async use<Result>(operation: (files: readonly File[]) => Promise<Result>): Promise<Result> {
      const snapshot = [...selected]
      if (closed || !snapshot.length) throw new Error('请先选择文件。')
      for (const file of snapshot) uses.set(file, (uses.get(file) ?? 0) + 1)
      let failed = false, failure: unknown
      try { return await operation(snapshot) }
      catch (cause) { failed = true; failure = cause; throw cause }
      finally {
        for (const file of snapshot) {
          const count = uses.get(file)! - 1
          if (count) uses.set(file, count)
          else uses.delete(file)
        }
        try { cleanup() }
        catch (cause) { if (failed) throw new AggregateError([failure, cause], '操作失败，且未能清理选择副本。'); throw cause }
      }
    },
    close() {
      closed = true
      for (const file of selected) retired.add(file)
      selected.clear()
      cleanup()
    },
  }
}
