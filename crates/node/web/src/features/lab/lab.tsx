import { Link } from '@tanstack/react-router'
import { ArrowRightLeft, FlaskConical, ShieldAlert } from 'lucide-react'
import { useExperiments } from '@vibeface/client/lib/experiments.ts'
import { Button } from '@/components/ui/button'
import { SettingsGroup, SettingsRow } from '@/components/app/kit'
import { webPlatform } from '@/lib/platform'

/** The single place where browser-only experimental features are exposed. */
export function Lab() {
  const [experiments, setExperiments] = useExperiments(webPlatform.storage)
  return <div className="mx-auto grid w-full max-w-(--page-width) gap-6">
    <section className="grid gap-2 rounded-xl border bg-card p-4">
      <h2 className="text-heading font-semibold">实验室</h2>
      <p className="text-meta text-muted-foreground">这里的功能还在测试。关闭实验室后，消息、设备、发现、同步和普通公益传输不受影响。</p>
    </section>
    <SettingsGroup title="实验开关" footer="开关只保存在这个浏览器，不会改变这台设备的身份或连接设置。">
      <SettingsRow icon={FlaskConical} label="启用实验功能" description={experiments ? '已开启，实验功能会在对应页面出现' : '默认关闭，普通功能保持不变'}>
        <input type="checkbox" role="switch" className="switch" aria-label="启用实验功能" checked={experiments} onChange={(event) => setExperiments(event.target.checked)} />
      </SettingsRow>
    </SettingsGroup>
    <SettingsGroup title="当前实验" footer="实验功能可能只支持本地测试链，不代表正式付款或生产服务。">
      <SettingsRow icon={ArrowRightLeft} label="付费传输（本地测试）" description={experiments ? '已在文件传输页面显示' : '开启实验功能后显示'}>
        {experiments ? <Button asChild type="button" size="sm" variant="outline"><Link to="/transfers">打开</Link></Button> : <span>已关闭</span>}
      </SettingsRow>
      <SettingsRow icon={ShieldAlert} label="实验边界" description="付费传输仍受设备权限和本地测试链设置控制" />
    </SettingsGroup>
  </div>
}
