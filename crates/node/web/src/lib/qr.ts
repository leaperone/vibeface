import qrcode from 'qrcode-generator'

export function createEnrollmentQr(link: string) {
  if (!/^[\x21-\x7e]+$/.test(link) || new TextEncoder().encode(link).length > 2000) throw new Error('邀请链接超过可扫码容量。请使用短引用链接或复制链接。')
  const code = qrcode(0, 'M')
  code.addData(link, 'Byte')
  try { code.make() } catch { throw new Error('邀请链接超过可扫码容量。请使用短引用链接或复制链接。') }
  return { src: code.createDataURL(4, 16), size: code.getModuleCount() * 4 + 32 }
}
