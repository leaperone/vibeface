import { Laptop, Monitor, Server, Smartphone, type LucideIcon } from 'lucide-react-native'
import { phoneName } from '@vibeface/client/workspace/labels.ts'
import type { Member } from '@/data/types'

/** As on the web: the connected computer, another computer, a phone, or else a browser; this phone is a phone whatever it is called. */
export const deviceIcon = (member: Pick<Member, 'kind' | 'name' | 'current' | 'entry'>): LucideIcon =>
  member.kind === 'node' ? member.entry ? Laptop : Server : member.current || phoneName(member.name) ? Smartphone : Monitor
