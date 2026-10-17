export type Account = { account_id: string; username: string; network_id: string }
export type Session = { account: Account; csrf: string }

const networkError = "连接中断，尚未确认操作结果。请刷新或核对操作记录，不要重复派发。"

let csrf = ""
let account: Account | null = null
let epoch = 0

export async function register(credentials: {
  username: string
  password: string
}): Promise<Session> {
  await write("/api/register", credentials)
  return login(credentials)
}

export async function login(credentials: {
  username: string
  password: string
}): Promise<Session> {
  const data = await write("/api/login", credentials)
  return storeSession(parseSession(data), true)
}

export async function me(): Promise<Session> {
  const data = await read("/api/me")
  return storeSession(parseSession(data), false)
}

export async function logout(): Promise<void> {
  await write("/api/logout", {})
  epoch += 1
  csrf = ""
  account = null
}

export async function rpc(
  method: string,
  fields: Record<string, unknown> = {},
): Promise<unknown> {
  return write("/api/rpc", { method, ...fields })
}

export function openEvents(
  clientId: string,
  onSignal: (event: MessageEvent) => void,
): () => void {
  const source = new EventSource(
    `/api/events?client_id=${encodeURIComponent(clientId)}`,
  )
  const notify = (event: Event) => {
    onSignal(event as MessageEvent)
  }
  source.addEventListener("update", notify)
  source.addEventListener("reset", notify)
  source.addEventListener("message", notify)
  return () => {
    source.removeEventListener("update", notify)
    source.removeEventListener("reset", notify)
    source.removeEventListener("message", notify)
    source.close()
  }
}

function storeSession(session: Session, boundary: boolean): Session {
  const changed =
    account?.account_id !== session.account.account_id ||
    account?.network_id !== session.account.network_id
  account = session.account
  csrf = session.csrf
  if (boundary || changed) epoch += 1
  return session
}

async function read(path: string): Promise<unknown> {
  return request(path, undefined)
}

async function write(path: string, payload: unknown): Promise<unknown> {
  return request(path, payload)
}

async function request(path: string, payload: unknown): Promise<unknown> {
  const started = epoch
  const snapshot = account
  for (let attempt = 0; ; attempt += 1) {
    const token = csrf
    let response: Response
    try {
      response = await fetch(path, {
        method: payload === undefined ? "GET" : "POST",
        credentials: "same-origin",
        headers:
          payload === undefined
            ? {}
            : {
                "Content-Type": "application/json",
                "X-CSRF-Token": token,
              },
        body: payload === undefined ? undefined : JSON.stringify(payload),
      })
    } catch {
      throw new Error(networkError)
    }
    const data: unknown = await response.json().catch(() => ({}))
    if (started !== epoch) {
      throw new Error("账户已切换，已忽略上一个账户的请求结果。")
    }
    if (!response.ok) {
      if (
        response.status === 403 &&
        payload !== undefined &&
        attempt === 0 &&
        snapshot?.account_id &&
        snapshot.network_id
      ) {
        const auth = await refreshSession()
        if (started !== epoch) {
          throw new Error("账户已切换，已忽略上一个账户的请求结果。")
        }
        if (
          auth?.account &&
          (auth.account.account_id !== snapshot.account_id ||
            auth.account.network_id !== snapshot.network_id)
        ) {
          epoch += 1
          csrf = ""
          account = null
          throw new Error("账户已切换，请重新登录后核对原操作。")
        }
        if (
          auth?.account.account_id === snapshot.account_id &&
          auth.account.network_id === snapshot.network_id &&
          auth.csrf !== token
        ) {
          csrf = auth.csrf
          account = auth.account
          continue
        }
      }
      const error = new Error(errorMessage(data, response.status)) as Error & {
        status?: number
      }
      error.status = response.status
      throw error
    }
    return data
  }
}

async function refreshSession(): Promise<Session | null> {
  try {
    const response = await fetch("/api/me", {
      method: "GET",
      credentials: "same-origin",
      mode: "same-origin",
    })
    if (!response.ok) return null
    const data: unknown = await response.json().catch(() => null)
    return data ? readSession(data) : null
  } catch {
    return null
  }
}

function parseSession(data: unknown): Session {
  const session = readSession(data)
  if (!session) throw new Error("入口没有返回完整的账户会话，请重新登录。")
  return session
}

function readSession(data: unknown): Session | null {
  if (!data || typeof data !== "object") return null
  const record = data as { account?: unknown; csrf?: unknown }
  if (!record.account || typeof record.account !== "object") return null
  if (typeof record.csrf !== "string" || !record.csrf) return null
  const accountRecord = record.account as Partial<Account>
  if (
    typeof accountRecord.account_id !== "string" ||
    typeof accountRecord.username !== "string" ||
    typeof accountRecord.network_id !== "string"
  ) {
    return null
  }
  return {
    account: {
      account_id: accountRecord.account_id,
      username: accountRecord.username,
      network_id: accountRecord.network_id,
    },
    csrf: record.csrf,
  }
}

function errorMessage(data: unknown, status: number): string {
  if (data && typeof data === "object") {
    const record = data as { error?: unknown; message?: unknown }
    if (typeof record.error === "string") return record.error
    if (record.error && typeof record.error === "object") {
      const nested = (record.error as { message?: unknown }).message
      if (typeof nested === "string") return nested
    }
    if (typeof record.message === "string") return record.message
  }
  return `请求失败（${status}）`
}
