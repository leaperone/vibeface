import { useEffect, useState } from 'react'
import { faceOf, sameAvatar, type Avatar, type Eyes, type Face, type Hue, type Outline } from './face.ts'

export const outlineNames: Record<Outline, string> = { round: '圆', squircle: '方圆', drop: '水滴', bean: '豆子', cloud: '云朵' }
export const eyeNames: Record<Eyes, string> = { dot: '圆点眼', pill: '竖条眼', oval: '椭圆眼', wide: '大眼' }
export const hueNames: Record<Hue, string> = {
  20: '珊瑚红', 45: '橘色', 100: '黄色', 130: '黄绿', 155: '绿色', 180: '青绿',
  205: '青色', 230: '天蓝', 255: '蓝色', 285: '紫色', 315: '品红', 345: '玫红',
}

/** The face shown with one part changed, as a full avatar to write. */
export const withPart = (face: Face, part: Partial<Avatar>): Avatar => ({ outline: face.outline, eyes: face.eyes, hue: face.hue, ...part })

/** A face in words, for screen readers: "圆、圆点眼、蓝色". */
export const avatarWords = (face: Avatar) => `${outlineNames[face.outline]}、${eyeNames[face.eyes]}、${hueNames[face.hue]}`

/** A face change in words; a face left to the id is still named, as it shows. */
export const avatarChangeWords = (botId: string, before: Avatar | null, after: Avatar | null) =>
  `长相：${avatarWords(faceOf(botId, before))} → ${avatarWords(faceOf(botId, after))}${after ? '' : '（默认）'}`

/**
 * Where a pending panel write stands against the bot list: its face shows (`landed`), the face it was written over
 * still shows so it can go again (`unsent`), or something else changed the face since (`superseded`).
 */
export function avatarWrite(current: Avatar | null, before: Avatar | null, after: Avatar | null): 'landed' | 'unsent' | 'superseded' {
  return sameAvatar(current, after) ? 'landed' : sameAvatar(current, before) ? 'unsent' : 'superseded'
}

/**
 * A pick on show: while `writing` it waits for the Node; once written it stays while the bot list still shows one of
 * the `stale` faces it was picked over, so a slow or failed refresh neither flashes an old face nor reports a failure.
 */
export type Look = { bot_id: string; avatar: Avatar | null; stale: (Avatar | null)[]; writing: boolean }

/** The pick still on show for `bot`, or null once the list has moved on or another bot is open. */
export const liveLook = (look: Look | null, bot: { bot_id: string; avatar: Avatar | null }): Look | null =>
  look && look.bot_id === bot.bot_id && (look.writing || look.stale.some((face) => sameAvatar(bot.avatar, face))) ? look : null

/**
 * Picking a bot's face: the pick shows at once and stays while it is written and until the list shows it; a failed
 * write goes back to the stored face and reports through `failed`. A pick while one is being written is ignored.
 * `write` gets the face the user saw, so the Node can refuse it when the face changed meanwhile.
 */
export function useAvatarPick(bot: { bot_id: string; avatar: Avatar | null }, write: (before: Avatar | null, after: Avatar | null) => Promise<void>, failed: (error: unknown) => void) {
  const [state, setState] = useState<Look | null>(null)
  const look = liveLook(state, bot)
  useEffect(() => { if (state && !look) setState(null) }, [state, look])
  const avatar = look ? look.avatar : bot.avatar
  const pick = (next: Avatar | null) => {
    if (look?.writing || sameAvatar(next, avatar)) return
    const picked: Look = { bot_id: bot.bot_id, avatar: next, stale: [bot.avatar, avatar].filter((face) => !sameAvatar(face, next)), writing: true }
    setState(picked)
    write(avatar, next).then(
      () => setState((now) => now === picked ? { ...picked, writing: false } : now),
      (error: unknown) => { setState((now) => now === picked ? null : now); failed(error) },
    )
  }
  return { avatar, face: faceOf(bot.bot_id, avatar), pending: look?.writing === true, pick }
}
