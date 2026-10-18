import { useRef, type ChangeEvent, type KeyboardEvent } from 'react'
import { ArrowUpIcon } from 'lucide-react'
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
}: {
  value: string
  hint: string
  disabled: boolean
  busy: boolean
  onChange: (value: string) => void
  onSubmit: () => void
}) {
  const formRef = useRef<HTMLFormElement>(null)
  return (
    <form
      ref={formRef}
      onSubmit={(event) => {
        event.preventDefault()
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
          placeholder="描述目标，或询问某个任务的进展…"
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
            type="submit"
            variant="default"
            size="icon-sm"
            disabled={disabled || busy || !value.trim()}
            aria-busy={busy || undefined}
            aria-label="发送消息"
            className="ms-auto size-10 rounded-full"
          >
            {busy ? <Spinner aria-hidden="true" /> : <ArrowUpIcon aria-hidden="true" />}
          </InputGroupButton>
        </InputGroupAddon>
      </InputGroup>
      {hint ? <p id="manager-hint">{hint}</p> : null}
    </form>
  )
}
