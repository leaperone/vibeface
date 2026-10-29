import { useState } from 'react'
import { faceOf, sameAvatar, type Avatar, type Eyes, type Face, type Hue, type Outline } from './face.ts'

export const outlineNames: Record<Outline, string> = { round: '圆', squircle: '方圆', drop: '水滴', bean: '豆子', cloud: '云朵' }
export const eyeNames: Record<Eyes, string> = { dot: '圆点眼', pill: '竖条眼', oval: '椭圆眼', wide: '大眼' }
export const hueNames: Record<Hue, string> = {
  20: '珊瑚红', 45: '橘色', 100: '黄色', 130: '黄绿', 155: '绿色', 180: '青绿',
  205: '青色', 230: '天蓝', 255: '蓝色', 285: '紫色', 315: '品红', 345: '玫红',
}

/** The face shown with one part changed, as a full avatar to write. */
export const withPart = (face: Face, part: Partial<Avatar>): Avatar => ({ outline: face.outline, eyes: face.eyes, hue: face.hue, ...part })

/**
 * Picking a bot's face: the pick shows at once and stays while it is written; a failed write goes back to the stored
 * face and reports through `failed`. A pick while one is being written is ignored.
 */
export function useAvatarPick(bot: { bot_id: string; avatar: Avatar | null }, write: (avatar: Avatar | null) => Promise<void>, failed: (error: unknown) => void) {
  const [shown, setShown] = useState<{ avatar: Avatar | null } | null>(null)
  const avatar = shown ? shown.avatar : bot.avatar
  const pick = (next: Avatar | null) => {
    if (shown || sameAvatar(next, bot.avatar)) return
    setShown({ avatar: next })
    write(next).catch(failed).finally(() => setShown(null))
  }
  return { avatar, face: faceOf(bot.bot_id, avatar), pending: shown !== null, pick }
}
