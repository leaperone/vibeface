import { useEffect, useState } from 'react'
import { faceOf, sameAvatar, type Avatar } from './face.ts'
import { liveLook, type Faced, type Look } from './pick.ts'

/**
 * Picking a bot's face: the pick shows at once and stays while it is written and until the face's version moves on; a
 * failed write goes back to the stored face and reports through `failed`. A pick while one is being written is ignored.
 * `write` gets the version the user saw, so the Node can refuse it once the face changed meanwhile.
 */
export function useAvatarPick(bot: Faced, write: (version: string | null, after: Avatar | null) => Promise<void>, failed: (error: unknown) => void) {
  const [state, setState] = useState<Look | null>(null)
  const look = liveLook(state, bot)
  useEffect(() => { if (state && !look) setState(null) }, [state, look])
  const avatar = look ? look.avatar : bot.avatar
  const pick = (next: Avatar | null) => {
    if (look?.writing || sameAvatar(next, avatar)) return
    const picked: Look = { bot_id: bot.bot_id, avatar: next, version: bot.avatar_version, writing: true }
    setState(picked)
    write(bot.avatar_version, next).then(
      () => setState((now) => now === picked ? { ...picked, writing: false } : now),
      (error: unknown) => { setState((now) => now === picked ? null : now); failed(error) },
    )
  }
  return { avatar, face: faceOf(bot.bot_id, avatar), pending: look?.writing === true, pick }
}
