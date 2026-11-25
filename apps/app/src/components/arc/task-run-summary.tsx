import { Wifi,WifiOff } from 'lucide-react';
import { StatusPill } from './product-kit';
import styles from './task-compositions.module.css';

/** Arc agent-run summary with product counts. No simulated elapsed time or progress. */
export function TaskRunSummary({ attention, working, waiting, online }: { attention: number | null; working: number | null; waiting: number | null; online: boolean }) {
  return <section className={styles.summary} aria-label="个人任务总览"><h2 className="sr-only">任务进展</h2><dl className={styles.metrics}>{[{ label: '需要留意', value: attention },{ label: '进行中', value: working },{ label: '等待中', value: waiting }].map(item => <div key={item.label} className={styles.metric}><dt>{item.label}</dt><dd><strong>{item.value ?? '待确认'}</strong></dd></div>)}</dl><StatusPill tone={online ? 'neutral' : 'warning'} icon={online ? Wifi : WifiOff}>{online ? '网络在线' : '连接待恢复'}</StatusPill></section>
}
