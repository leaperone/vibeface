import { useRef, type ChangeEvent, type KeyboardEvent } from 'react'
import { ArrowUpIcon, SquareIcon } from 'lucide-react'
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
  reportResults,
  onReportResults,
  onChange,
  onSubmit,
  onStop,
}: {
  value: string
  hint: string
  disabled: boolean
  busy: boolean
  reportResults: boolean
  onReportResults: (value: boolean) => void
  onChange: (value: string) => void
  onSubmit: () => void
  onStop?: () => void
}) {
  const formRef = useRef<HTMLFormElement>(null)
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
          aria-label="给机器人发消息"
          aria-describedby={hint ? 'manager-hint' : undefined}
          title="Enter 发送，Shift+Enter 换行"
          className="min-h-14 max-h-[min(12rem,30dvh)] overflow-y-auto p-3"
          onChange={(event: ChangeEvent<HTMLTextAreaElement>) => onChange(event.target.value)}
          onKeyDown={(event: KeyboardEvent<HTMLTextAreaElement>) => {
            if (isSubmitKey(event.nativeEvent)) {
              event.preventDefault()
              formRef.current?.requestSubmit()
            }
          }}
        />
        <InputGroupAddon align="block-end" className="pt-1">
          <InputGroupButton
            type={onStop ? 'button' : 'submit'}
            variant="default"
            size="icon-sm"
            disabled={!onStop && (disabled || busy || !value.trim())}
            aria-busy={busy || undefined}
            aria-label={onStop ? '停止回复' : '发送消息'}
            className="ms-auto size-10 rounded-full"
            onClick={onStop}
          >
            {onStop ? <SquareIcon aria-hidden="true" /> : busy ? <Spinner aria-hidden="true" /> : <ArrowUpIcon aria-hidden="true" />}
          </InputGroupButton>
        </InputGroupAddon>
      </InputGroup>
      {hint ? <p id="manager-hint">{hint}</p> : null}
      <label className="mt-3 flex min-h-11 items-start gap-2 text-sm">
        <input
          type="checkbox"
          className="mt-1 size-4 accent-primary focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-ring disabled:cursor-not-allowed disabled:opacity-50"
          checked={reportResults}
          disabled={disabled || busy}
          onChange={(event) => onReportResults(event.target.checked)}
        />
        <span>完成后告诉我</span>
      </label>
      <p className="text-xs text-muted-foreground">只影响这条消息，不改后续授权。</p>
    </form>
  )
}
