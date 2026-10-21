import { useRef, type ChangeEvent, type KeyboardEvent, type ReactNode } from 'react'
import { ArrowUpIcon, PaperclipIcon, SquareIcon, XIcon } from 'lucide-react'
import { Spinner } from '@/components/ui/spinner'
import { isSubmitKey } from '../protocol'

import {
  InputGroup,
  InputGroupAddon,
  InputGroupButton,
  InputGroupTextarea,
} from '@/components/ui/input-group'

export function PromptInput({
  value,
  hint,
  disabled,
  busy,
  onChange,
  onSubmit,
  onStop,
  files = [],
  onFiles,
  children,
}: {
  value: string
  hint: string
  disabled: boolean
  busy: boolean
  onChange: (value: string) => void
  onSubmit: () => void
  onStop?: () => void
  files?: File[]
  onFiles?: (files: File[]) => void
  children?: ReactNode
}) {
  const formRef = useRef<HTMLFormElement>(null)
  const pickerRef = useRef<HTMLInputElement>(null)
  return (
    <form
      ref={formRef}
      onSubmit={(event) => {
        event.preventDefault()
        if (onStop) return
        if (disabled || busy || !value.trim()) return
        onSubmit()
      }}
    >
      <InputGroup>
        <InputGroupTextarea
          id="message-text"
          name="text"
          required
          maxLength={100000}
          value={value}
          placeholder="发消息"
          disabled={disabled || busy}
          aria-label="给助手发消息"
          aria-describedby={hint ? 'manager-hint' : undefined}
          title="Enter 发送，Shift+Enter 换行"
          className="min-h-14 max-h-[min(12rem,30dvh)] overflow-y-auto px-4 py-3"
          onChange={(event: ChangeEvent<HTMLTextAreaElement>) => onChange(event.target.value)}
          onKeyDown={(event: KeyboardEvent<HTMLTextAreaElement>) => {
            if (isSubmitKey(event.nativeEvent)) {
              event.preventDefault()
              formRef.current?.requestSubmit()
            }
          }}
        />
        <InputGroupAddon align="block-end" className="flex-wrap ps-0.5 pt-1">
          {onFiles ? <>
            <input ref={pickerRef} type="file" multiple hidden onChange={(event) => { onFiles([...files, ...event.target.files ?? []]); event.target.value = '' }} />
            <InputGroupButton type="button" variant="ghost" size="icon-sm" className="size-11 rounded-full" aria-label="添加附件" title="添加附件" disabled={disabled || busy} onClick={() => pickerRef.current?.click()}>
              <PaperclipIcon aria-hidden="true" />
            </InputGroupButton>
            {files.map((file, index) => <span key={`${file.name}:${index}`} className="flex max-w-48 min-w-0 items-center gap-1 rounded-full bg-muted ps-3 text-meta">
              <span className="truncate">{file.name}</span>
              <button type="button" className="grid size-8 shrink-0 place-items-center rounded-full hover:bg-background" aria-label={`移除 ${file.name}`} disabled={busy} onClick={() => onFiles(files.filter((_, at) => at !== index))}><XIcon aria-hidden="true" className="size-3.5" /></button>
            </span>)}
          </> : null}
          <InputGroupButton
            type={onStop ? 'button' : 'submit'}
            variant={onStop ? 'outline' : 'default'}
            size="icon-sm"
            disabled={!onStop && (disabled || busy || !value.trim())}
            aria-busy={busy || undefined}
            aria-label={onStop ? '停止' : '发送'}
            title={onStop ? '停止' : '发送'}
            className="ms-auto size-11 rounded-full"
            onClick={onStop}
          >
            {onStop ? <SquareIcon aria-hidden="true" /> : busy ? <Spinner aria-hidden="true" /> : <ArrowUpIcon aria-hidden="true" />}
          </InputGroupButton>
        </InputGroupAddon>
      </InputGroup>
      <div className="grid gap-1 pt-2 text-meta text-muted-foreground">
        {hint ? <p id="manager-hint">{hint}</p> : null}
        {children}
      </div>
    </form>
  )
}
