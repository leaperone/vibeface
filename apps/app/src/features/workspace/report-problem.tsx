import { FormField } from "@/components/arc/form-field"
import { BusyIcon,InlineError,SettingsRow,useAction } from "@/components/arc/product-kit"
import { useNavigate } from "@tanstack/react-router"
import { lastFailure,problemReport,subscribeFailures } from "@vibeface/client/lib/error-ref.ts"
import { useWorkspace } from "@vibeface/client/workspace/context.tsx"
import { Button } from "@vibeface/uiarc/button/button.tsx"
import { Checkbox } from "@vibeface/uiarc/checkbox/checkbox.tsx"
import { Drawer,DrawerContent } from "@vibeface/uiarc/drawer/drawer.tsx"
import { Textarea } from "@vibeface/uiarc/textarea/textarea.tsx"
import { MessageSquareWarning,Send } from "lucide-react"
import { useRef,useState,useSyncExternalStore } from "react"

const noPrimary = "还没有主 Bot。先在 Bot 信息里设一个。"

/**
 * 报告问题: the description, with the conversation last open and the latest error number when the person keeps
 * them, goes to the primary bot as an ordinary message; the page then opens that bot to follow its answer.
 */
export function ReportProblem() {
  const workspace = useWorkspace()
  const navigate = useNavigate()
  const failure = useSyncExternalStore(subscribeFailures, lastFailure, lastFailure)
  const trigger = useRef<HTMLButtonElement>(null)
  const [open, setOpen] = useState(false)
  const [text, setText] = useState("")
  const [withConversation, setWithConversation] = useState(true)
  const [withRef, setWithRef] = useState(true)
  const action = useAction()
  const primary = workspace.bots.find((bot) => bot.conversation_id === workspace.primary)
  const conversation = workspace.conversationId || failure?.conversation_id
  const conversationName = workspace.bots.find((bot) => bot.conversation_id === conversation)?.name
  const busy = action.busy !== null
  const submit = () => void action.run("send", async () => {
    if (!primary) throw new Error(noPrimary)
    const message = problemReport({ description: text, conversation_id: withConversation ? conversation : undefined, ref: withRef ? failure?.ref : undefined })
    await workspace.sendMessage(message, [], [], { target: { kind: "bot_main", bot_id: primary.bot_id } })
    setOpen(false); setText("")
    void navigate({ to: "/b/$botId", params: { botId: primary.bot_id } })
  })
  return <>
    <SettingsRow ref={trigger} icon={MessageSquareWarning} label="报告问题" onClick={() => { action.clear(); setOpen(true) }} />
    <Drawer open={open} onOpenChange={setOpen}>
      <DrawerContent title="报告问题" description={primary ? `发给主 Bot「${primary.name}」` : undefined} side="bottom" className="gap-0 pb-[env(safe-area-inset-bottom)] sm:mx-auto sm:max-w-md sm:rounded-t-2xl sm:border-x" onCloseAutoFocus={(event) => { event.preventDefault(); trigger.current?.focus({ preventScroll: true }) }}>
        <form className="grid gap-4 p-4" noValidate onSubmit={(event) => { event.preventDefault(); submit() }}>
          <FormField>
            <Textarea label="遇到了什么问题" id="problem-text" rows={4} maxLength={4000} required value={text} disabled={busy} onChange={(event) => setText(event.target.value)} />
          </FormField>
          {conversation || failure ? <div className="grid gap-2">
            {conversation ? <Checkbox label={conversationName ? `附带会话「${conversationName}」` : "附带当前会话"} checked={withConversation} disabled={busy} onCheckedChange={(next) => setWithConversation(next === true)} /> : null}
            {failure ? <Checkbox label={`附带错误编号 #${failure.ref}`} checked={withRef} disabled={busy} onCheckedChange={(next) => setWithRef(next === true)} /> : null}
          </div> : null}
          <InlineError error={action.error} />
          {!primary ? <p role="status" className="text-sm text-secondary">{noPrimary}</p> : !workspace.writable ? <p role="status" className="text-sm text-secondary">{workspace.writeNotice || "连上后就能发送。"}</p> : null}
          <div className="flex justify-end gap-2">
            <Button type="button" variant="ghost" onClick={() => setOpen(false)}>取消</Button>
            <Button type="submit" disabled={busy || !workspace.writable || !primary || !text.trim()} aria-busy={busy || undefined}><BusyIcon busy={busy} icon={Send} />发送</Button>
          </div>
        </form>
      </DrawerContent>
    </Drawer>
  </>
}
