import { EmptyState,InlineError,SettingsGroup,SettingsRow } from '@/components/arc/product-kit'
import { RoutedAction } from "@/components/arc/routed-action"
import { webPlatform } from '@/lib/platform'
import { useDelivery } from '@vibeface/client/delivery/client.ts'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { Switch } from "@vibeface/uiarc/switch/switch.tsx"
import { ArrowRightLeft,FlaskConical } from 'lucide-react'

export function Lab() {
  const { ready } = useDevice()
  return ready ? <ConnectedLab key={`${ready.generation}:${ready.entry.device_id}`} connection={ready} /> : <EmptyState icon={FlaskConical} title="设备暂时离线" description="重新连接后可查看实验设置和传输记录。" />
}

function ConnectedLab({ connection }: { connection: NonNullable<ReturnType<typeof useDevice>['ready']> }) {
  const delivery = useDelivery(connection, webPlatform)
  const status = delivery.status
  return <div className="mx-auto grid w-full max-w-(--page-width) gap-6">
    <p className="text-sm text-secondary">这里集中放置还在试用的功能，默认关闭。消息、聊天附件、设备连接和同步沿用原来的设置。</p>
    {delivery.error ? <InlineError error={delivery.error} /> : null}
    <SettingsGroup title="文件传输" footer="只控制当前连接的电脑。关闭会暂停传输和中转，保留传输记录与已保存的文件。付费订单的原截止时间不会延后。">
      <SettingsRow icon={FlaskConical} label="文件传输实验" description="加密免费转发与付费测试，付费部分只使用本地测试代币">
        {status ? <Switch aria-label="文件传输实验" checked={status.enabled} disabled={!delivery.canManage || delivery.busy !== null} onCheckedChange={next => { const checked = next === true;
          void delivery.configure({ enabled: checked, charity_enabled: status.charity_enabled, paid_enabled: status.paid_enabled })
         }} /> : <span role="status">正在读取设置</span>}
      </SettingsRow>
      <SettingsRow icon={ArrowRightLeft} label="发送、接收和传输记录" description="关闭后仍可查看记录、保存已收到的文件">
        <RoutedAction size="sm" variant="secondary" to="/settings/lab/transfers">打开文件传输</RoutedAction>
      </SettingsRow>
    </SettingsGroup>
    {status && !delivery.canManage ? <p className="text-sm text-secondary">只有管理设备能更改实验设置。</p> : null}
  </div>
}
