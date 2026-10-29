import { Laptop, Monitor, Smartphone, Tablet, type LucideIcon } from 'lucide-react-native'
import { deviceForm, type DeviceForm } from '@vibeface/client/workspace/labels.ts'
import type { Member } from '@/data/types'
import { tablet } from '@/platform'

const icons: Record<DeviceForm, LucideIcon> = { computer: Laptop, tablet: Tablet, phone: Smartphone, browser: Monitor }

/** As on the web: a computer, a tablet, a phone, or else a browser; this device is what it is whatever it is called. */
export const deviceIcon = (member: Pick<Member, 'kind' | 'name' | 'current'>): LucideIcon =>
  icons[member.current ? tablet ? 'tablet' : 'phone' : deviceForm(member.kind, member.name)]
