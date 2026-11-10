/** Match `vibeface_control::MAX_CONTROL_PACKET`. */
export const MAX_CONTROL_PACKET = 1024 * 1024

/** Match `vibeface_secure_ffi::lan::MAX_FRAME_BYTES`. */
export const MAX_LAN_FRAME = 1024 * 1024

/** Local HTTP proxy bodies fail closed at the same 1 MiB cap as control packets. */
export const MAX_HTTP_BODY = MAX_CONTROL_PACKET

/** Base64 JSON line wrapping a LAN frame, matching Core `MAX_LINE_BYTES`. */
export const MAX_LAN_LINE = Math.ceil(MAX_LAN_FRAME * 4 / 3) + 4096
