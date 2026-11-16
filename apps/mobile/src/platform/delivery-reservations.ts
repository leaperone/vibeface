import type { ReserveDeliveryIntent } from '@vibeface/client/delivery/types.ts'

/** One JS runtime owns the synchronous module-level MMKV store. No cross-runtime exclusion. */
export const nativeDeliveryReservations: ReserveDeliveryIntent = async (_key, reserve) => reserve()
