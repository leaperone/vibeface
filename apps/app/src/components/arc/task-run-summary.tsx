import { Wifi,WifiOff } from 'lucide-react';
import { StatusPill } from './product-kit';
import styles from './task-compositions.module.css';

/** Arc agent-run summary with product counts. No simulated elapsed time or progress. */
export function TaskRunSummary({ attention, working, waiting, online }: { attention: number | null; working: number | null; waiting: number | null; online: boolean }) {
  const lead = attention === null ? '当前连接的任务数量尚未确认。' : attention ? `${attention} 项需要你先处理，其他工作会继续留在这里。` : working ? `${working} 项正在推进，等待中的工作会单独留在视线里。` : '从每个 Bot 的下一步开始。'
  return <section className={styles.summary} aria-label="个人任务总览"><div className={styles.summaryHeader}><h2>任务与后台回复</h2><StatusPill tone={online ? 'success' : 'warning'} icon={online ? Wifi : WifiOff}>{online ? '网络在线' : '连接待恢复'}</StatusPill></div><p className="text-sm text-secondary">{lead}</p><dl className={styles.metrics}>{[{ label: '需要留意', value: attention },{ label: '进行中', value: working },{ label: '等待中', value: waiting }].map(item => <div key={item.label} className={styles.metric}><dt>{item.label}</dt><dd><strong>{item.value ?? '待确认'}</strong></dd></div>)}</dl></section>
}
