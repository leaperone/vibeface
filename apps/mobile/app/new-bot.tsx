import { Link, Stack, useRouter } from 'expo-router'
import { Check, ChevronRight, FolderOpen, LoaderCircle } from 'lucide-react-native'
import { useState } from 'react'
import { Pressable, View } from 'react-native'
import { KeyboardAwareScrollView } from 'react-native-keyboard-controller'
import { useDevice } from '@vibeface/client/identity/context.tsx'
import { folderLabels, folderSetup } from '@vibeface/client/workspace/bot-settings.ts'
import { nodeName } from '@vibeface/client/workspace/labels.ts'
import { projectName } from '@vibeface/client/workspace/questions.ts'
import type { BotProject } from '@vibeface/client/workspace/types.ts'
import { ErrorText, useAction } from '@/components/action'
import { Button } from '@/components/ui/button'
import { Icon } from '@/components/ui/icon'
import { Input } from '@/components/ui/input'
import { Text } from '@/components/ui/text'
import { Textarea } from '@/components/ui/textarea'
import { useLive } from '@/data/live'
import { cn } from '@/lib/utils'

/** A new bot, as the web form: a name, what it does and a folder it works in; an unconfirmed one is retried unchanged. */
export default function NewBot() {
  const shared = useLive()
  const { directory, ready } = useDevice()
  const router = useRouter()
  const pending = shared?.pendingBot ?? null
  const [name, setName] = useState(pending?.name ?? '')
  const [instructions, setInstructions] = useState(pending?.instructions ?? '')
  const [project, setProject] = useState<BotProject | undefined>(pending?.project)
  const { busy, error, run } = useAction()
  if (!shared) return null
  const lookup = { nodes: shared.nodes, directory }
  const projects = shared.nodes.flatMap((node, index) => node.revoked || !node.online || !node.capabilities?.manager ? []
    : (node.capabilities.roots ?? []).map((cwd) => ({ project: { target_node: node.device_id, cwd }, label: `${nodeName(node.device_id, lookup, `设备 ${index + 1}`)} · ${projectName(cwd)}` })))
  const same = (item?: BotProject) => item?.target_node === project?.target_node && item?.cwd === project?.cwd
  const options = [{ project: undefined, label: '不选' }, ...projects]
  if (pending?.project && !projects.some((item) => same(item.project))) options.push({ project: pending.project, label: `${nodeName(pending.project.target_node, lookup, '原来的设备')} · ${projectName(pending.project.cwd)}` })
  const create = () => run('create', async () => {
    const bot = await shared.createBot(pending ? { name: pending.name, instructions: pending.instructions, project: pending.project } : { name, instructions, project })
    if (bot) router.replace(`/b/${bot.bot_id}`)
  })
  const here = ready?.entry.device_id
  const setup = ready && here ? folderSetup(shared.nodes, ready.principal, here) : undefined
  const locked = Boolean(pending) || busy !== null
  const disabled = busy !== null || !shared.writable || !(pending?.name ?? name).trim()
  return <KeyboardAwareScrollView bottomOffset={24} keyboardShouldPersistTaps="handled" contentInsetAdjustmentBehavior="automatic" contentContainerClassName="w-full max-w-3xl self-center gap-6 p-4 pb-10">
    {/* In the header, so the keyboard never covers it. */}
    <Stack.Screen options={{ headerRight: () => <Button variant="ghost" className="px-3" disabled={disabled} onPress={() => void create()}>
      {busy ? <Icon as={LoaderCircle} /> : null}<Text className="font-semibold">{pending ? '再试一次' : '创建'}</Text>
    </Button> }} />
    {pending ? <Text role="status" className="rounded-lg bg-muted p-3 text-meta">{pending.submission?.state === 'rejected' ? '上次没建成，确认后可以修改。' : '上次没确认是否建好，内容保持不变。'}</Text> : null}
    <View className="gap-2">
      <Text className="text-meta font-medium text-muted-foreground">名称</Text>
      <Input accessibilityLabel="名称" placeholder="例如：项目 Bot" maxLength={512} value={name} editable={!locked} onChangeText={setName} />
    </View>
    <View className="gap-2">
      <Text className="text-meta font-medium text-muted-foreground">它负责什么 <Text className="text-meta text-muted-foreground/70">可选</Text></Text>
      <Textarea accessibilityLabel="它负责什么" placeholder="例如：帮我维护项目，回答问题并检查修改。" className="min-h-28" maxLength={8192} value={instructions} editable={!locked} onChangeText={setInstructions} />
    </View>
    <View className="gap-2">
      <Text className="text-meta font-medium text-muted-foreground">{folderLabels.project} <Text className="text-meta text-muted-foreground/70">可选</Text></Text>
      <View accessibilityRole="radiogroup" className="overflow-hidden rounded-xl border border-border bg-card shadow-xs dark:border-line-strong">
        {options.map((item, index) => {
          const selected = same(item.project)
          return <Pressable key={item.project ? `${item.project.target_node}:${item.project.cwd}` : 'none'} accessibilityRole="radio" accessibilityState={{ checked: selected, disabled: locked }}
            disabled={locked} onPress={() => setProject(item.project)}
            className={cn('min-h-12 flex-row items-center gap-3 px-4 py-2 active:bg-muted', index > 0 && 'border-t border-border', locked && !selected && 'opacity-50')}>
            <Icon as={FolderOpen} className="size-4 text-muted-foreground" />
            <Text numberOfLines={1} className="min-w-0 flex-1">{item.label}</Text>
            {selected ? <Icon as={Check} className="size-4" /> : null}
          </Pressable>
        })}
      </View>
      {projects.length || pending?.project ? null : <View className="flex-row flex-wrap items-center gap-x-2">
        <Text className="text-meta text-muted-foreground">{setup?.add ? '还没有可用的文件夹' : '设备能运行 Bot 后就能选'}</Text>
        {/* This phone edits only the AI settings of the device it connects through. */}
        {pending || !setup ? null : setup.device === here ? <Link href="/ai" asChild><Pressable accessibilityRole="link" className="min-h-11 flex-row items-center gap-0.5">
          <Text className="text-meta font-medium">{setup.add ? '去添加' : '去设置'}</Text><Icon as={ChevronRight} className="size-3.5" />
        </Pressable></Link> : setup.device ? null : <Text className="text-meta text-muted-foreground">在管理设备上设置</Text>}
      </View>}
    </View>
    <ErrorText error={error} />
    {!shared.writable ? <Text role="status" className="text-meta text-muted-foreground">{shared.writeNotice || '连上后就能创建。'}</Text> : null}
  </KeyboardAwareScrollView>
}
