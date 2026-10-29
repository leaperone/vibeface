import { NativeModule, requireNativeModule } from 'expo'

/** A `_vibeface._udp` service on this LAN: its TXT `t` and the IPv4 `ip:port` it resolved to. */
export type LanService = { tag: string; addrs: string[] }
type Events = { onChange(event: { services: LanService[] }): void }
/** Browses while `onChange` has a listener, and sends every resolved service each time the set changes. */
declare class LanBrowser extends NativeModule<Events> {}

export default requireNativeModule<LanBrowser>('LanBrowser')
