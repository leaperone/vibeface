import { Link, createFileRoute } from '@tanstack/react-router'
import { ArrowRight, ArrowUpRight } from 'lucide-react'
import { SiteLayout } from '@/components/site-layout'

export const Route = createFileRoute('/')({
  head: () => ({
    meta: [
      { title: 'Vibeface · 用聊天指挥自己的电脑' },
      { name: 'description', content: '复用本机 Codex 或 Claude Code，让 Bot 协调自己的电脑，并在授权设备间查看共享任务历史。' },
      { name: 'theme-color', content: '#080b09' },
    ],
    links: [{ rel: 'canonical', href: 'https://www.vibeface.dev/' }],
  }),
  component: HomePage,
})

function HomePage() {
  return (
    <SiteLayout>
      <div className="landing">
        <section className="landing-hero" aria-labelledby="hero-title">
          <div className="hero-copy-block">
            <p className="hero-kicker">自己的设备，自己的 Agent <span aria-hidden="true">/</span> Nightly</p>
            <h1 id="hero-title">用聊天，指挥<br />自己的电脑。</h1>
            <p className="hero-lede">复用本机已安装并登录的 Codex 或 Claude Code。Bot 协调，选定电脑执行，阶段与结果回到原聊天。模型服务与额度由你准备。</p>
            <div className="hero-actions">
              <Link className="button button-primary" to="/docs/$" params={{ _splat: 'install' }}>开始安装 <ArrowRight size={16} aria-hidden="true" /></Link>
              <Link className="button button-quiet" to="/docs/$" params={{ _splat: 'overview' }}>先了解 Vibeface <ArrowRight size={16} aria-hidden="true" /></Link>
            </div>
            <ul className="hero-proof" aria-label="Vibeface 的核心边界">
              <li><span>01</span><strong>本机执行</strong><small>复用已登录 Agent</small></li>
              <li><span>02</span><strong>签名历史</strong><small>带因果依赖的 DAG</small></li>
              <li><span>03</span><strong>权限可见</strong><small>先知道谁能读到内容</small></li>
            </ul>
          </div>

          <figure className="hero-figure" aria-label="Bot 协调、本机 Agent 执行与共享历史的关系示意">
            <div className="hero-visual">
              <div className="visual-grid" aria-hidden="true" />
              <div className="visual-glow" aria-hidden="true" />
              <div className="visual-readout" aria-hidden="true"><span>PERSONAL NETWORK / 01</span><strong>READY TO HAND OFF</strong></div>
              <div className="orbit orbit-one" aria-hidden="true" />
              <div className="orbit orbit-two" aria-hidden="true" />
              <div className="orbit orbit-three" aria-hidden="true" />
              <div className="hero-logo"><img src="/logo.svg" width="144" height="144" alt="" /></div>
              <a className="visual-node visual-node-top" href="/docs/bot-and-execution"><span>Bot 协调</span><small>理解需求，派发工作</small></a>
              <a className="visual-node visual-node-left" href="/docs/bot-and-execution#工作如何回到原聊天"><span>本机 Agent 执行</span><small>在你选定的电脑上</small></a>
              <a className="visual-node visual-node-right" href="/docs/history-sync"><span>共享历史</span><small>带签名的 DAG 记录</small></a>
            </div>
            <figcaption>关系示意 · 任务阶段与结果回到同一段 Bot 聊天。</figcaption>
          </figure>
        </section>

        <section className="landing-section landing-story" aria-labelledby="story-title">
          <p className="section-eyebrow">01 / 工作流</p>
          <h2 id="story-title">一条需求，留在同一段聊天。</h2>
          <p className="story-prompt"><span>示例需求</span>检查这个项目的失败测试，并说明需要怎样修改。</p>
          <div className="story-frame">
            <div className="story-frame-meta"><span>WORK LOOP / 01</span><span>PRIVATE NETWORK</span></div>
            <ol className="work-story" role="list">
              <li><h3>把需求交给 Bot</h3><p>交代目标、项目和希望使用的设备。Bot 结合这段聊天的历史组织工作。</p></li>
              <li><h3>选定电脑执行</h3><p>工作交给已授权、已准备好项目的电脑，由那里的 Agent 读取文件、运行工具。</p></li>
              <li><h3>回原聊天看结果</h3><p>阶段和持久结果回到原聊天。结果不确定时先核对原任务，不直接重发。</p></li>
            </ol>
            <div className="story-frame-result"><span>RETURNED TO CHAT</span><strong>阶段与持久结果回到原聊天</strong><small>完整逐字流式进度不作为共享历史保存。</small></div>
          </div>
        </section>

        <section className="landing-system" id="system" aria-labelledby="system-title">
          <div className="system-intro">
            <p className="section-eyebrow">02 / 结构</p>
            <h2 id="system-title">保留上下文，<br />原有工具继续用。</h2>
            <p>增加长期 Bot 协调和个人设备间的共享历史，执行仍交给本机的 Agent。</p>
            <a className="text-link" href="/docs/bot-and-execution">看看分工怎么安排 <ArrowRight size={16} aria-hidden="true" /></a>
          </div>
          <p className="system-manifesto">协调可以迁移，<em>执行留在你选择的电脑。</em></p>
          <div className="system-details">
            <article><h3>Bot 与执行分开</h3><p>协调与执行可以分别选择产品。复用自己电脑上的 Codex 或 Claude Code，各自准备程序、登录和项目。</p></article>
            <article><h3>共享 Bot 历史</h3><div><p>带签名和因果依赖的 DAG 同步 Bot 聊天、设置、任务阶段和结果。项目、依赖、登录与原生 SDK 状态不会自动迁移，完整逐字进度不作为共享历史保存。</p><a className="text-link" href="/docs/history-sync">了解同步范围 <ArrowRight size={16} aria-hidden="true" /></a></div></article>
            <article><h3>换台电脑继续</h3><p>另一台 Node 先同步，再由你选择“交给这台设备处理”，用共享记录建立本机 Agent 上下文。历史回放不会启动旧工作，断网时仍需核对任务状态。</p></article>
          </div>
        </section>

        <section className="landing-section landing-comparison" aria-labelledby="comparison-title">
          <p className="section-eyebrow">03 / 选择</p>
          <h2 id="comparison-title">已经有 Agent，还需要这一层吗？</h2>
          <p className="section-copy">原生工具也有远程与自动化入口。按你需要的工作方式选择。</p>
          <dl className="choice-list">
            <div><dt>直接用 Codex 或 Claude Code</dt><dd>你已在终端、IDE 或官方入口完成工作，只需要该产品自己的流程。</dd></div>
            <div><dt>了解 OpenClaw</dt><dd>你更关注自托管 Gateway，希望从常用聊天应用接入 Agent。</dd></div>
            <div><dt>选择 Vibeface</dt><dd>你需要长期 Bot 协调、选择自己的执行电脑，并在授权设备间查看共享历史。</dd></div>
          </dl>
          <a className="text-link" href="/docs/comparison">比较适合你的用法 <ArrowRight size={16} aria-hidden="true" /></a>
        </section>

        <section className="landing-section landing-trust" aria-labelledby="trust-title">
          <p className="section-eyebrow">04 / 边界</p>
          <h2 id="trust-title">先知道谁能读到内容。</h2>
          <div className="trust-content">
            <dl className="trust-facts">
              <div><dt>每台设备有自己的身份</dt><dd>设备各用独立密钥，新设备加入由管理设备核对并批准。</dd></div>
              <div><dt>连接验证身份并加密</dt><dd>只负责转发的公共入口搬运密文，不因转发加入个人网络。</dd></div>
              <div><dt>配置的凭证留在本机</dt><dd>Provider 配置不随历史同步。主动发进聊天的秘密仍会作为正文同步。</dd></div>
            </dl>
            <aside className="trust-boundary" aria-label="使用前需要了解的权限边界">
              <p>授权 Node 与所选模型服务仍会处理任务内容。请只使用你信任的设备与网页。</p>
              <p>传输加密不等于磁盘静态加密。项目目录检查不是操作系统沙箱，Codex 和 Claude Code 可完全访问宿主机。</p>
              <a className="text-link" href="/docs/privacy-security">阅读隐私与权限说明 <ArrowRight size={16} aria-hidden="true" /></a>
            </aside>
          </div>
        </section>

        <section className="landing-section landing-start" aria-labelledby="start-title">
          <p className="section-eyebrow">05 / 开始</p>
          <h2 id="start-title">从一台电脑开始。</h2>
          <p className="section-copy">准备一台支持的 Mac 或 Linux、已安装并登录的 Agent，以及自己的模型服务和项目目录。</p>
          <ol className="start-steps" role="list">
            <li><a href="/docs/install">安装并启动 Node <ArrowUpRight size={16} aria-hidden="true" /></a></li>
            <li><a href="/docs/node#配置管理-agent">配置 Agent 与可用目录 <ArrowUpRight size={16} aria-hidden="true" /></a></li>
            <li><a href="/docs/node#创建机器人并处理结果">创建 Bot，交代第一项工作 <ArrowUpRight size={16} aria-hidden="true" /></a></li>
          </ol>
          <p className="nightly-note">当前为 Nightly。正式版前不保留协议、数据格式和 API 的向后兼容。<a href="https://github.com/leaperone/vibeface/releases">查看 Nightly Releases <ArrowUpRight size={14} aria-hidden="true" /></a></p>
        </section>
      </div>
    </SiteLayout>
  )
}
