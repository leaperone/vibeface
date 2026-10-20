import { Paperclip } from 'lucide-react'
import { useRef, useState } from 'react'
import { Button } from '@/components/ui/button'
import { Card, CardContent } from '@/components/ui/card'
import { useWorkspace } from '@/features/workspace/context'
import { formatSize } from '@/features/workspace/model'
import { ActionButton } from '@/features/workspace/present'

const signatures: Array<[number[], string]> = [[[0x89, 0x50, 0x4e, 0x47], 'png'], [[0xff, 0xd8, 0xff], 'jpg'], [[0x47, 0x49, 0x46], 'gif'], [[0x25, 0x50, 0x44, 0x46], 'pdf'], [[0x50, 0x4b, 0x03, 0x04], 'zip']]

/** Messages keep only the hash, so the name comes from the content: a few known formats, else text or bin. */
function extension(bytes: Uint8Array) {
  const known = signatures.find(([magic]) => magic.every((byte, index) => bytes[index] === byte))
  if (known) return known[1]
  try { new TextDecoder('utf-8', { fatal: true }).decode(bytes); return 'txt' } catch { return 'bin' }
}

/** A long result, or a file the user attached (`attachment`). */
export function Artifact({ hash, size, attachment = false }: { hash: string; size: number; attachment?: boolean }) {
  const workspace = useWorkspace()
  const [content, setContent] = useState('')
  const [nextOffset, setNextOffset] = useState<number | null>(0)
  const [opened, setOpened] = useState(false)
  const cursor = useRef<number | null>(0)
  const inFlight = useRef<number | null>(null)
  const [readingGeneration, setReadingGeneration] = useState<number | null>(null)
  const reading = readingGeneration === workspace.generation
  const readNext = async () => {
    if (cursor.current === null || inFlight.current === workspace.generation) return
    const generation = workspace.generation
    inFlight.current = generation
    setReadingGeneration(generation)
    try {
      const page = await workspace.readArtifact(hash, cursor.current)
      if (!workspace.isCurrent()) return
      if (page.size !== size) throw new Error('产物大小与消息引用不符，已停止读取。')
      cursor.current = page.next_offset
      setContent((previous) => previous + page.text)
      setNextOffset(page.next_offset)
      setOpened(true)
    } finally { if (inFlight.current === generation) inFlight.current = null; setReadingGeneration(owner => owner === generation ? null : owner) }
  }
  return <Card className="w-full min-w-0 gap-0 py-0">
    <CardContent className="space-y-3 p-3">
      <div className="flex flex-wrap items-center gap-2">
        <span className="mr-auto flex items-center gap-1.5 text-sm">{attachment ? <Paperclip aria-hidden="true" className="size-4" /> : null}{attachment ? '附件' : '完整结果'} · {formatSize(size)}</span>
        {!opened ? <ActionButton disabled={!workspace.connectionOnline || reading} action={() => workspace.run(async () => { if (nextOffset === 0) await readNext(); else setOpened(true) })}>{attachment ? '打开' : '打开结果'}</ActionButton> : <Button type="button" variant="ghost" onClick={() => setOpened(false)}>收起结果</Button>}
        <ActionButton disabled={!workspace.connectionOnline} action={() => workspace.run(async () => {
          const parts: Uint8Array<ArrayBuffer>[] = []
          let offset: number | null = 0
          while (offset !== null) {
            const page = await workspace.readArtifactBytes(hash, offset)
            if (!workspace.isCurrent()) return
            if (page.size !== size) throw new Error('产物大小与消息引用不符，已停止下载。')
            parts.push(page.bytes)
            offset = page.next_offset
          }
          const whole = new Uint8Array(size)
          parts.reduce((at, part) => { whole.set(part, at); return at + part.length }, 0)
          const url = URL.createObjectURL(new Blob([whole]))
          const link = document.createElement('a')
          link.href = url
          link.download = `${attachment ? 'attachment' : 'result'}-${hash.slice(0, 12)}.${extension(whole)}`
          link.click()
          window.setTimeout(() => URL.revokeObjectURL(url), 1000)
        })}>下载</ActionButton>
      </div>
      {opened ? <>
        <pre className="max-h-80 overflow-auto whitespace-pre-wrap break-words text-sm">{content || '结果为空。'}</pre>
        {nextOffset !== null ? <ActionButton disabled={!workspace.connectionOnline || reading} action={() => workspace.run(readNext)}>读取后续内容</ActionButton> : <p className="text-xs text-muted-foreground">已读取完整结果。</p>}
      </> : null}
    </CardContent>
  </Card>
}
