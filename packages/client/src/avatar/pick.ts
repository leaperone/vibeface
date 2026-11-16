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

/** What a pick needs of a bot: its face and the version that names it. */
export type Faced = { bot_id: string; avatar: Avatar | null; avatar_version: string | null }

/**
 * Where a pending panel write stands against the bot list: its face shows (`landed`), the face is still at the version
 * it was signed over so it can go again (`unsent`), or the face changed since (`superseded`), even if it changed back.
 */
export function avatarWrite(current: Pick<Faced, 'avatar' | 'avatar_version'>, version: string | null, after: Avatar | null): 'landed' | 'unsent' | 'superseded' {
  return sameAvatar(current.avatar, after) ? 'landed' : current.avatar_version === version ? 'unsent' : 'superseded'
}

/** A pick on show: while `writing` it waits for the Node, then stays while the list still shows the `version` it was picked over. */
export type Look = { bot_id: string; avatar: Avatar | null; version: string | null; writing: boolean }

/** The pick still on show for `bot`, or null once the face has a new version or another bot is open. */
export const liveLook = (look: Look | null, bot: Faced): Look | null =>
  look && look.bot_id === bot.bot_id && (look.writing || bot.avatar_version === look.version) ? look : null
