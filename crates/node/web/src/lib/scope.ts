export type IdentityScope = Readonly<{ account_id: string; space_id: string; device_id: string }>

export function scopeKey(scope: IdentityScope, key: string) {
  if (!scope) throw new Error("账户、空间或设备身份缺失，已暂停读取本地记录。")
  const parts = [scope.account_id, scope.space_id, scope.device_id]
  if (parts.some(part => typeof part !== "string" || !part || part.length > 256)) throw new Error("账户、空间或设备身份缺失，已暂停读取本地记录。")
  return `v3.${JSON.stringify([...parts, key])}`
}

export function sameScope(left: IdentityScope, right: IdentityScope) {
  return left.account_id === right.account_id && left.space_id === right.space_id && left.device_id === right.device_id
}

export function parseIdentityScope(value: unknown): IdentityScope {
  if (value === null || typeof value !== 'object' || Array.isArray(value) || !('account_id' in value) || !('space_id' in value) || !('device_id' in value) || typeof value.account_id !== 'string' || typeof value.space_id !== 'string' || typeof value.device_id !== 'string' || !/^acc2_[a-f0-9]{64}$/.test(value.account_id) || !/^spc2_[a-f0-9]{64}$/.test(value.space_id) || !/^dev2_[a-f0-9]{64}$/.test(value.device_id) || Object.keys(value).length !== 3) throw new Error('已验证的账户、空间或设备范围格式无效。')
  return { account_id: value.account_id, space_id: value.space_id, device_id: value.device_id }
}
