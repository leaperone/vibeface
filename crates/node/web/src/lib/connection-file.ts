export async function readConnectionFile(file: Pick<File, 'size' | 'text'>): Promise<string> {
  if (!file.size || file.size > 64 * 1024) throw new Error('请选择不超过 64 KiB 的连接文件。')
  return file.text()
}

export function downloadConnectionFile(content: string): void {
  const url = URL.createObjectURL(new Blob([content], { type: 'application/json' }))
  const link = document.createElement('a')
  link.href = url
  link.download = 'vibeface-connection.json'
  link.click()
  setTimeout(() => URL.revokeObjectURL(url), 1000)
}
