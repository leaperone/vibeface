import type { ReserveDeliveryIntent } from '@vibeface/client/delivery/types.ts'

export function webDeliveryReservations(locks: Pick<LockManager, 'request'> | undefined): ReserveDeliveryIntent {
  return async (key, reserve) => {
    if (!locks) throw new Error('这个浏览器不能安全保留发送记录。请更新浏览器或使用原生应用。')
    // Return callback failures as rejected promises so the lock manager releases ownership.
    return locks.request(`vibeface-delivery-intent:${key}`, { mode: 'exclusive' }, async () => reserve())
  }
}
