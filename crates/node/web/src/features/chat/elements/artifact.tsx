import { Download, Eye, EyeOff, File } from 'lucide-react'
import { useRef, useState } from 'react'
import { Button } from '@/components/ui/button'
import { Card, CardContent } from '@/components/ui/card'
import { useWorkspace } from '@vibeface/client/workspace/context.tsx'
import { formatSize } from '@vibeface/client/workspace/model.ts'
import { ActionButton } from '@/features/workspace/present'
import type { Attachment } from '@vibeface/client/chat/types.ts'

/** A file the user attached: read it here as text, or download it under its own name. */
export function Artifact({ file: { hash, size, name } }: { file: Attachment }) {
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
      <div className="flex items-center gap-1">
        <span className="mr-auto flex min-w-0 flex-1 items-center gap-1.5 text-sm">
          <File aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" />
          <span className="min-w-0 truncate" title={name}>{name}</span>
          <span className="shrink-0 text-meta text-muted-foreground">{formatSize(size)}</span>
        </span>
        {!opened ? <ActionButton variant="ghost" label="打开" disabled={!workspace.connectionOnline || reading} action={() => workspace.run(async () => { if (nextOffset === 0) await readNext(); else setOpened(true) }, '打开')}><Eye aria-hidden="true" /></ActionButton>
          : <Button type="button" variant="ghost" size="icon" aria-label="收起" title="收起" onClick={() => setOpened(false)}><EyeOff aria-hidden="true" /></Button>}
        <ActionButton variant="ghost" label="下载" disabled={!workspace.connectionOnline} action={() => workspace.run(async () => {
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
          const digest = [...new Uint8Array(await crypto.subtle.digest('SHA-256', whole))].map((byte) => byte.toString(16).padStart(2, '0')).join('')
          if (digest !== hash) throw new Error('下载内容与原请求不符。')
          const url = URL.createObjectURL(new Blob([whole]))
          const link = document.createElement('a')
          link.href = url
          link.download = name
          link.click()
          window.setTimeout(() => URL.revokeObjectURL(url), 1000)
        }, '下载')}><Download aria-hidden="true" /></ActionButton>
      </div>
      {opened ? <>
        <pre className="max-h-80 overflow-auto whitespace-pre-wrap break-words text-sm">{content || '文件是空的'}</pre>
        {nextOffset !== null ? <ActionButton disabled={!workspace.connectionOnline || reading} action={() => workspace.run(readNext, '读取后续内容')}>读取后续内容</ActionButton> : <p className="text-xs text-muted-foreground">已显示全部内容</p>}
      </> : null}
    </CardContent>
  </Card>
}
