import { ArrowRightLeft, Download, FlaskConical, LoaderCircle, MessageSquareWarning, ScrollText, Upload } from 'lucide-react-native'
import { useState } from 'react'
import { ScrollView, View } from 'react-native'
import { sendLogs, type SendResult } from '@vibeface/client/lib/web-log.ts'
import { useExperiments, experimentsCopy } from '@vibeface/client/lib/experiments.ts'
import { ErrorText } from '@/components/action'
import { SettingsGroup, SettingsRow } from '@/components/kit'
import { Button } from '@/components/arc/button'
import { Icon } from '@/components/arc/icon'
import { Switch } from '@/components/arc/switch'
import { Text } from '@/components/arc/text'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { mobilePlatform } from '@/platform'
import { useLive } from '@/data/live'
import { useRun } from '@/lib/use-run'

/** What one press of 发送 did, in a line. */
const sentText = ({ sent, left, connected }: SendResult) =>
  !left ? (sent ? `已发送 ${sent} 条` : '全部已同步')
  : !connected ? `未连接，连上后自动发送 ${left} 条`
  : `已发送 ${sent} 条，其余 ${left} 条稍后自动发送`

export default function Lab() {
  const device = useDevice()
  const shared = useLive()
  const [experiments, setExperiments] = useExperiments(mobilePlatform.storage)
  const { busy, error, status, run } = useRun()
  const [sending, setSending] = useState(false)
  const [sent, setSent] = useState<SendResult | null>(null)
  const sendNow = () => {
    setSending(true); setSent(null)
    void sendLogs().then(setSent).finally(() => setSending(false))
  }
  const canInspect = Boolean(shared) && (device.ready?.principal.can_manage === true || device.ready?.principal.source === 'local_session')
  const exportDiagnostics = () => void run('diagnostics', async () => {
    const files = mobilePlatform.files
    if (!files || !shared) throw new Error('设备暂时离线，不能导出诊断包。')
    const bundle = await shared.diagnostics(Date.now() - 86_400_000)
    const name = `vibeface-diagnostics-${new Date().toISOString().slice(0, 10)}.json`
    await files.save(new TextEncoder().encode(JSON.stringify(bundle ?? null)), name)
  }, '诊断包已交给系统保存。')
  return <ScrollView contentInsetAdjustmentBehavior="automatic" contentContainerClassName="w-full max-w-3xl self-center gap-6 p-6 pb-12">
    <SettingsGroup title={experimentsCopy.group}>
      <SettingsRow href="/transfers" icon={ArrowRightLeft} label="文件传输" />
      <SettingsRow icon={FlaskConical} label={experimentsCopy.label} description={experiments ? experimentsCopy.on : experimentsCopy.hint}>
        <Switch accessibilityLabel={experimentsCopy.label} value={experiments} onValueChange={setExperiments} />
      </SettingsRow>
    </SettingsGroup>
    {shared ? <SettingsGroup title="问题排查">
      <SettingsRow href="/report" icon={MessageSquareWarning} label="报告问题" />
      <SettingsRow icon={Upload} label="发送日志到节点">
        <Button variant="secondary" size="sm" accessibilityLabel={sending ? '日志发送中' : '发送日志到节点'} disabled={sending} onPress={sendNow}>
          <Icon as={sending ? LoaderCircle : Upload} /><Text>{sending ? '发送中' : '发送'}</Text>
        </Button>
        {sent ? <Text role="status" className={sent.left ? 'basis-full ps-11 text-sm text-muted' : 'basis-full ps-11 text-sm text-success'}>{sentText(sent)}</Text> : null}
      </SettingsRow>
      {canInspect ? <><SettingsRow href="/logs" icon={ScrollText} label="日志" />
      <SettingsRow icon={Download} label="诊断包" description="不含登录信息和加入链接">
        <Button variant="secondary" size="sm" disabled={busy !== null} onPress={exportDiagnostics}>
          <Icon as={busy === 'diagnostics' ? LoaderCircle : Download} /><Text>{busy === 'diagnostics' ? '导出中' : '导出'}</Text>
        </Button>
        {status ? <Text role="status" className="basis-full ps-11 text-sm text-success">{status}</Text> : null}
        {error ? <View className="basis-full ps-11"><ErrorText error={error} /></View> : null}
      </SettingsRow></> : null}
    </SettingsGroup> : null}
  </ScrollView>
}
