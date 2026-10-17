"use client"

import { useEffect, useRef, useState, type ChangeEvent } from "react"
import { login, me, register, type Session } from "@/lib/node"
import { Alert, AlertDescription } from "@/components/ui/alert"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Field, FieldGroup, FieldLabel } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { Spinner } from "@/components/ui/spinner"
import { nodeName } from "./labels"
import { loadInfo } from "./info"
import { consumeNotice } from "./notice"
import { errorText, statusOf } from "./reconcile"
import type { EntryInfo } from "./types"

export function AuthScreen({ onSession }: { onSession: (session: Session) => void }) {
  const onSessionRef = useRef(onSession)
  onSessionRef.current = onSession
  const [registerMode, setRegisterMode] = useState(false)
  const [username, setUsername] = useState("")
  const [password, setPassword] = useState("")
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState("")
  const [notice, setNotice] = useState("")
  const [info, setInfo] = useState<EntryInfo>({})
  const [connection, setConnection] = useState("连接中")
  const [origin, setOrigin] = useState("")

  useEffect(() => {
    setOrigin(window.location.origin)
    setNotice(consumeNotice())
    let cancelled = false
    void (async () => {
      try {
        const next = await loadInfo()
        if (cancelled) return
        setInfo(next)
        setConnection("入口可达")
        try {
          const session = asSession(await me())
          if (cancelled) return
          if (session) onSessionRef.current(session)
          else setConnection("入口可达 · 等待登录")
        } catch (caught) {
          if (cancelled) return
          if (statusOf(caught) === 401) setConnection("入口可达 · 等待登录")
          else {
            setConnection("入口连接失败")
            setError(errorText(caught))
          }
        }
      } catch (caught) {
        if (cancelled) return
        setConnection("入口连接失败")
        setError(errorText(caught))
      }
    })()
    return () => {
      cancelled = true
    }
  }, [])

  return (
    <>
      <a className="sr-only" href="#main">跳到主要内容</a>
      <header>
        <a href="/" aria-label="Vibeface 首页">vibeface</a>
        <p>
          <span data-connection={connection === "入口连接失败" ? "offline" : "online"}>{connection}</span>
          {info.node_id ? <span> {nodeName(info.node_id)}</span> : null}
        </p>
      </header>
      {notice ? <Alert><AlertDescription>{notice}</AlertDescription></Alert> : null}
      {error ? <Alert variant="destructive"><AlertDescription>{error}</AlertDescription></Alert> : null}
      <main id="main" tabIndex={-1}>
        <section>
          <p>你的设备，你的工作</p>
          <h1>从任意入口，继续同一个会话。</h1>
          <p>与本地调度 Agent 聊天，查看任务和执行结果。设备离线后，可从其他已同步的设备继续。</p>
          <p><span>当前入口</span> <code>{origin}</code></p>
        </section>
        <Card>
          <CardHeader>
            <CardTitle>{registerMode ? "创建此入口的账户" : "登录此入口"}</CardTitle>
            <CardDescription>
              {registerMode
                ? "账户用于访问你的个人设备网络。已配对的设备沿用原网络身份。"
                : "使用此入口上的账户登录。换到其他地址时需要重新登录。"}
            </CardDescription>
          </CardHeader>
          <CardContent>
            <form
              onSubmit={(event) => {
                event.preventDefault()
                if (!event.currentTarget.reportValidity()) return
                setBusy(true)
                setError("")
                const credentials = { username: username.trim(), password }
                void (async () => {
                  try {
                    const result = registerMode ? await register(credentials) : await login(credentials)
                    const session = asSession(result) ?? asSession(await me())
                    if (!session) throw new Error("入口没有返回完整的账户会话，请重新登录。")
                    setPassword("")
                    onSessionRef.current(session)
                  } catch (caught) {
                    setError(errorText(caught))
                  } finally {
                    setBusy(false)
                  }
                })()
              }}
            >
              <FieldGroup>
                <Field>
                  <FieldLabel htmlFor="username">用户名</FieldLabel>
                  <Input id="username" name="username" autoComplete="username" required maxLength={128} value={username} onChange={(event: ChangeEvent<HTMLInputElement>) => setUsername(event.target.value)} />
                </Field>
                <Field>
                  <FieldLabel htmlFor="password">密码</FieldLabel>
                  <Input
                    id="password"
                    name="password"
                    type="password"
                    autoComplete={registerMode ? "new-password" : "current-password"}
                    required
                    maxLength={1024}
                    value={password}
                    onChange={(event: ChangeEvent<HTMLInputElement>) => setPassword(event.target.value)}
                  />
                </Field>
                <Button type="submit" disabled={busy} aria-busy={busy}>
                  {busy ? <Spinner /> : null}
                  {registerMode ? "创建账户并登录" : "登录"}
                </Button>
              </FieldGroup>
            </form>
            <Button
              type="button"
              variant="ghost"
              onClick={() => {
                setRegisterMode((current) => !current)
                setError("")
              }}
            >
              {registerMode ? "已有账户？返回登录" : "第一次使用？创建账户"}
            </Button>
          </CardContent>
        </Card>
      </main>
      <footer>Vibeface {info.version ?? ""}<span>本地运行 · 由你控制</span></footer>
    </>
  )
}

function asSession(value: unknown): Session | null {
  if (!value || typeof value !== "object") return null
  const body = value as Session
  if (!body.account?.account_id || !body.account.network_id || typeof body.csrf !== "string" || !body.csrf) return null
  return body
}
