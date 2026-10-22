import { useMemo, useRef, useState } from 'react'
import type { BotRecord, ChatMessage, Conversation, Question, Workspace, WorkspaceActions, WorkspaceSnapshot } from './types'

const minute = 60_000
const now = Date.now()

function bot(bot_id: string, name: string, last: string, ago: number): BotRecord {
  return {
    bot_id, conversation_id: `c-${bot_id}`, name, instructions: '', event_hash: `h-${bot_id}`,
    last_message: { text: last, role: 'assistant', created_at_ms: now - ago },
  }
}

function message(id: string, role: 'user' | 'assistant', text: string, ago: number): ChatMessage {
  return { message_id: id, role, text, event_hash: `e-${id}`, created_at_ms: now - ago }
}

const bots = [
  bot('main', '主 bot', '已经把 web 的依赖检查完了，没有过期的包。', 4 * minute),
  bot('mobile', '移动端', '要我先跑一次 expo export 吗？', 52 * minute),
  bot('infra', '服务器', '昨晚的备份正常。', 26 * 60 * minute),
]

const question: Question = {
  question_id: 'q-1', conversation_id: 'c-mobile', assistant: { bot_id: 'mobile', name: '移动端' }, severity: 'attention',
  text: '要在 iOS 模拟器上装一次开发版吗？', choices: [{ label: '装吧' }, { label: '先不装' }, { label: '只打包不安装' }],
  about: { node: 'mac', agent: 'codex', session_id: 's-1', cwd: '/Users/me/vibeface/apps/mobile', summary: '' },
  author: 'mac', created_at_ms: now - 50 * minute, answer: null,
}

const conversations: Record<string, Conversation> = {
  'c-main': {
    messages: [
      message('m1', 'user', '帮我检查一下 web 的依赖', 6 * minute),
      message('m2', 'assistant', '检查完了：\n\n- **react** 19.3.1，最新\n- **vite** 8.3.4，最新\n- `tailwindcss` 4.3.3\n\n```sh\npnpm outdated --filter vibeface-web\n```\n\n没有过期的包。', 4 * minute),
    ],
    questions: [], works: [], replying: false, status: null, stoppable: false, blocked: null,
  },
  'c-mobile': {
    messages: [
      message('m3', 'user', '移动端能跑起来了吗？', 55 * minute),
      message('m4', 'assistant', 'Metro 配置好了，`tsc` 也过了。要我先跑一次 expo export 吗？', 52 * minute),
    ],
    questions: [question], works: [], replying: false, status: null, stoppable: false, blocked: null,
  },
  'c-infra': {
    messages: [message('m5', 'assistant', '昨晚的备份正常。', 26 * 60 * minute)],
    questions: [], works: [], replying: false, status: null, stoppable: false, blocked: null,
  },
}

const initial: WorkspaceSnapshot = {
  session: { phase: 'unjoined' },
  connection: { state: 'online', entry: 'MacBook Pro' },
  bots,
  primary: 'c-main',
  unread: { 'c-mobile': 1 },
  questions: [question],
  members: [
    { device_id: 'mac', name: 'MacBook Pro', kind: 'node', admin: true, online: true, current: false },
    { device_id: 'pve', name: '公网服务器', kind: 'node', admin: false, online: true, current: false },
    { device_id: 'phone', name: '我的手机', kind: 'client', admin: false, online: true, current: true },
  ],
  conversations: {},
}

const wait = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms))

/** An in-memory network so every screen runs without a Node; replies stream back canned text. */
export function useMockWorkspace(): Workspace {
  const [snapshot, setSnapshot] = useState(initial)
  const replies = useRef(new Map<string, ReturnType<typeof setInterval>>())

  const actions = useMemo<WorkspaceActions>(() => {
    const patch = (id: string, change: (chat: Conversation) => Conversation) =>
      setSnapshot((current) => {
        const chat = current.conversations[id]
        return chat ? { ...current, conversations: { ...current.conversations, [id]: change(chat) } } : current
      })
    const finish = (id: string, status: ChatMessage['status']) => {
      clearInterval(replies.current.get(id))
      replies.current.delete(id)
      patch(id, (chat) => ({
        ...chat, replying: false, status: null, stoppable: false,
        messages: chat.messages.map((item) => item.state === 'streaming' ? { ...item, state: undefined, status } : item),
      }))
    }
    return {
      async join(_invitation, deviceName) {
        setSnapshot((current) => ({ ...current, session: { phase: 'joining', deviceName, code: null } }))
        await wait(1200)
        setSnapshot((current) => ({ ...current, session: { phase: 'joining', deviceName, code: '482913' } }))
        await wait(2500)
        setSnapshot((current) => current.session.phase === 'joining' ? { ...current, session: { phase: 'ready' } } : current)
      },
      cancelJoin() {
        setSnapshot((current) => ({ ...current, session: { phase: 'unjoined' } }))
      },
      open(id) {
        setSnapshot((current) => ({
          ...current,
          unread: { ...current.unread, [id]: 0 },
          conversations: current.conversations[id] ? current.conversations : { ...current.conversations, [id]: conversations[id] ?? { ...conversations['c-infra'], messages: [] } },
        }))
        return () => {}
      },
      async send(id, text, attachments) {
        const at = Date.now()
        const names = attachments.map((file) => file.name).join('、')
        patch(id, (chat) => ({
          ...chat, replying: true, status: '思考中', stoppable: true,
          messages: [...chat.messages, { ...message(`u-${at}`, 'user', names ? `${text}\n\n附件：${names}` : text, 0), created_at_ms: at }],
        }))
        await wait(900)
        const words = '收到。我先看一下项目结构，然后告诉你下一步。\n\n1. 读 `package.json`\n2. 检查 Metro 配置\n3. 跑一次类型检查'.split('')
        let shown = ''
        patch(id, (chat) => chat.replying ? { ...chat, status: '正在回复', messages: [...chat.messages, { ...message(`a-${at}`, 'assistant', '', 0), created_at_ms: at, state: 'streaming' }] } : chat)
        replies.current.set(id, setInterval(() => {
          shown += words.splice(0, 3).join('')
          patch(id, (chat) => ({ ...chat, messages: chat.messages.map((item) => item.state === 'streaming' ? { ...item, text: shown } : item) }))
          if (!words.length) finish(id, 'completed')
        }, 60))
      },
      async stop(id) {
        finish(id, 'stopped')
      },
      async answer(asked, text) {
        await wait(400)
        const answer = { message_id: `a-${asked.question_id}`, text, author: 'phone', created_at_ms: Date.now() }
        const settle = (item: Question) => item.question_id === asked.question_id ? { ...item, answer } : item
        setSnapshot((current) => ({
          ...current,
          questions: current.questions.map(settle),
          conversations: Object.fromEntries(Object.entries(current.conversations).map(([id, chat]) => [id, { ...chat, questions: chat.questions.map(settle) }])),
        }))
        return { status: 'answered', text }
      },
      disconnect() {
        setSnapshot({ ...initial, session: { phase: 'unjoined' } })
      },
    }
  }, [])

  return useMemo(() => ({ ...snapshot, actions }), [snapshot, actions])
}
