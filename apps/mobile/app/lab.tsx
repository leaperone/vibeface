import { ArrowRightLeft, Download, FlaskConical, LoaderCircle, ScrollText } from 'lucide-react-native'
import { ScrollView, View } from 'react-native'
import { useExperiments, experimentsCopy } from '@vibeface/client/lib/experiments.ts'
import { ErrorText } from '@/components/action'
import { SettingsGroup, SettingsRow } from '@/components/kit'
import { Button } from '@/components/ui/button'
import { Icon } from '@/components/ui/icon'
import { Switch } from '@/components/ui/switch'
import { Text } from '@/components/ui/text'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { mobilePlatform } from '@/platform'
import { useLive } from '@/data/live'
import { useRun } from '@/lib/use-run'

export default function Lab() {
  const device = useDevice()
  const shared = useLive()
  const [experiments, setExperiments] = useExperiments(mobilePlatform.storage)
  const { busy, error, status, run } = useRun()
  const canInspect = Boolean(shared) && (device.ready?.principal.can_manage === true || device.ready?.principal.source === 'local_session')
  const exportDiagnostics = () => void run('diagnostics', async () => {
    const files = mobilePlatform.files
    if (!files || !shared) throw new Error('设备暂时离线，不能导出诊断包。')
    const bundle = await shared.diagnostics(Date.now() - 86_400_000)
    const name = `vibeface-diagnostics-${new Date().toISOString().slice(0, 10)}.json`
    await files.save(new TextEncoder().encode(JSON.stringify(bundle ?? null)), name)
  }, '诊断包已交给系统保存。')
  return <ScrollView contentInsetAdjustmentBehavior="automatic" contentContainerClassName="w-full max-w-3xl self-center gap-6 p-4 pb-10">
    <SettingsGroup title={experimentsCopy.group}>
      <SettingsRow href="/transfers" icon={ArrowRightLeft} label="文件传输" />
      <SettingsRow icon={FlaskConical} label={experimentsCopy.label} description={experiments ? experimentsCopy.on : experimentsCopy.hint}>
        <Switch accessibilityLabel={experimentsCopy.label} value={experiments} onValueChange={setExperiments} />
      </SettingsRow>
    </SettingsGroup>
    {canInspect ? <SettingsGroup title="问题排查">
      <SettingsRow href="/logs" icon={ScrollText} label="日志" />
      <SettingsRow icon={Download} label="诊断包" description="不含登录信息和加入链接">
        <Button variant="outline" size="sm" disabled={busy !== null} onPress={exportDiagnostics}>
          <Icon as={busy === 'diagnostics' ? LoaderCircle : Download} /><Text>{busy === 'diagnostics' ? '导出中' : '导出'}</Text>
        </Button>
        {status ? <Text role="status" className="basis-full ps-11 text-meta text-success">{status}</Text> : null}
        {error ? <View className="basis-full ps-11"><ErrorText error={error} /></View> : null}
      </SettingsRow>
    </SettingsGroup> : null}
  </ScrollView>
}
