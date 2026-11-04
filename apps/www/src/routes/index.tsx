import { Link, createFileRoute } from '@tanstack/react-router'
import { ArrowRight, ArrowUpRight } from 'lucide-react'
import { SiteLayout } from '@/components/site-layout'

export const Route = createFileRoute('/')({
  head: () => ({
    meta: [
      { title: 'Vibeface · Your computers. One conversation.' },
      { name: 'description', content: 'Chat with a bot, choose where work runs, and keep the result in the same conversation across your own computers.' },
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
            <p className="hero-kicker">Your devices <span aria-hidden="true">/</span> Your agents <span aria-hidden="true">/</span> Nightly</p>
            <h1 id="hero-title">Your computers.<br />One conversation.</h1>
            <p className="hero-lede">Ask a bot to work on your Mac or Linux computer. Choose where it runs and read the result in the same chat.</p>
            <div className="hero-actions">
              <Link className="button button-primary" to="/docs/$" params={{ _splat: 'quick-start' }}>Start with one computer <ArrowRight size={16} aria-hidden="true" /></Link>
              <Link className="button button-quiet" to="/docs/$" params={{ _splat: 'overview' }}>Read the overview <ArrowRight size={16} aria-hidden="true" /></Link>
            </div>
            <ul className="hero-proof" aria-label="Vibeface foundations">
              <li><span>01</span><strong>Local execution</strong><small>Your Agent runs on your computer</small></li>
              <li><span>02</span><strong>Shared context</strong><small>Keep the work in one thread</small></li>
              <li><span>03</span><strong>Approved devices</strong><small>Choose who can connect</small></li>
            </ul>
          </div>

          <figure className="hero-figure" aria-label="A visual map of a conversation, the selected computer, and shared history">
            <div className="hero-visual">
              <div className="visual-grid" aria-hidden="true" />
              <div className="visual-glow" aria-hidden="true" />
              <div className="visual-readout" aria-hidden="true"><span>PRIVATE NETWORK / 01</span><strong>READY TO HAND OFF</strong></div>
              <div className="orbit orbit-one" aria-hidden="true" />
              <div className="orbit orbit-two" aria-hidden="true" />
              <div className="orbit orbit-three" aria-hidden="true" />
              <div className="hero-logo"><img src="/logo.svg" width="144" height="144" alt="" /></div>
              <a className="visual-node visual-node-top" href="/docs/bot-and-execution"><span>Conversation</span><small>Shape the request, then hand it off</small></a>
              <a className="visual-node visual-node-left" href="/docs/bot-and-execution#工作如何回到原聊天"><span>Selected computer</span><small>Your Agent uses local tools</small></a>
              <a className="visual-node visual-node-right" href="/docs/history-sync"><span>Shared history</span><small>Stages and results return</small></a>
            </div>
            <figcaption>One thread from request to result.</figcaption>
          </figure>
        </section>

        <section className="landing-section landing-story" aria-labelledby="story-title">
          <p className="section-eyebrow">The loop</p>
          <h2 id="story-title">The work stays together.</h2>
          <p className="story-prompt"><span>Example request</span>“Check the failing tests and tell me what to change.”</p>
          <div className="story-frame">
            <div className="story-frame-meta"><span>WORK LOOP / 01</span><span>PRIVATE NETWORK</span></div>
            <ol className="work-story" role="list">
              <li><h3>Ask the bot</h3><p>Share the goal, project, and computer you want to use. The bot uses the thread to plan the work.</p></li>
              <li><h3>Choose a computer</h3><p>An approved computer runs the work with its local Agent, files, and tools.</p></li>
              <li><h3>Read the result</h3><p>Updates and saved results return to the same chat. Check an unknown result before retrying.</p></li>
            </ol>
            <div className="story-frame-result"><span>RETURNED TO CHAT</span><strong>Your request and result, together.</strong><small>Live progress is temporary and does not sync.</small></div>
          </div>
        </section>

        <section className="landing-system" id="system" aria-labelledby="system-title">
          <div className="system-intro">
            <p className="section-eyebrow">The system</p>
            <h2 id="system-title">Keep the context.<br />Keep your tools.</h2>
            <p>Keep a bot's conversation across your computers. Work runs with the tools installed on each one.</p>
            <a className="text-link" href="/docs/bot-and-execution">See how work is split <ArrowRight size={16} aria-hidden="true" /></a>
          </div>
          <p className="system-manifesto">Continue the conversation elsewhere. <em>Choose where the work runs.</em></p>
          <div className="system-details">
            <article><h3>Conversation and execution</h3><p>Use a bot to coordinate work while Codex, Claude Code, pi, opencode, or Cursor runs it on the computer you choose.</p></article>
            <article><h3>Shared history</h3><div><p>Approved devices share conversations, settings, task updates, and saved results. Project files, logins, and Agent sessions stay on each computer.</p><a className="text-link" href="/docs/history-sync">See what syncs <ArrowRight size={16} aria-hidden="true" /></a></div></article>
            <article><h3>Continue elsewhere</h3><p>Open a synchronized thread on another computer and hand off the next turn. Replaying history never starts old work.</p></article>
          </div>
        </section>

        <section className="landing-section landing-comparison" aria-labelledby="comparison-title">
          <p className="section-eyebrow">The choice</p>
          <h2 id="comparison-title">Already have an Agent?</h2>
          <p className="section-copy">Choose the layer that matches how you work.</p>
          <dl className="choice-list">
            <div><dt>Use Codex or Claude Code</dt><dd>You only need the product's own terminal, editor, or hosted flow.</dd></div>
              <div><dt>Look at OpenClaw</dt><dd>You want a self-hosted service that connects agents to chat apps.</dd></div>
            <div><dt>Choose Vibeface</dt><dd>You want one thread that can send work to your own computers and show the result back.</dd></div>
          </dl>
          <a className="text-link" href="/docs/comparison">Compare the options <ArrowRight size={16} aria-hidden="true" /></a>
        </section>

        <section className="landing-section landing-trust" aria-labelledby="trust-title">
          <p className="section-eyebrow">The boundary</p>
          <h2 id="trust-title">Know where your work goes.</h2>
          <div className="trust-content">
            <dl className="trust-facts">
              <div><dt>Every device has its own identity</dt><dd>Each device has its own key. You approve new devices before they join.</dd></div>
              <div><dt>Connections are encrypted</dt><dd>A public entry point can relay ciphertext without joining your private network.</dd></div>
              <div><dt>Credentials stay local</dt><dd>AI service settings do not sync. A secret you send in chat becomes part of the shared conversation.</dd></div>
            </dl>
            <aside className="trust-boundary" aria-label="Permission boundaries to know before you start">
              <p>Approved computers and the model service you choose handle your task content. Use devices and websites you trust.</p>
              <p>Encrypted connections do not encrypt files on disk. Agents can run commands and edit files on the host. Project folder settings are not an operating system sandbox.</p>
              <a className="text-link" href="/docs/privacy-security">Read privacy and permissions <ArrowRight size={16} aria-hidden="true" /></a>
            </aside>
          </div>
        </section>

        <section className="landing-section landing-start" aria-labelledby="start-title">
          <p className="section-eyebrow">Start here</p>
          <h2 id="start-title">Begin with one computer.</h2>
          <p className="section-copy">Use a supported Mac or Linux computer, one installed Agent, a model service, and a project folder. Start with one conversation, then add another computer.</p>
          <ol className="start-steps" role="list">
            <li><a href="/docs/quick-start">Follow the quick start <span>Finish your first conversation</span> <ArrowUpRight size={16} aria-hidden="true" /></a></li>
            <li><a href="/docs/onboarding">Set up a device <span>Sign in and add computers</span> <ArrowUpRight size={16} aria-hidden="true" /></a></li>
            <li><a href="/docs/user-guide">Read the user guide <span>Use tasks and switch computers</span> <ArrowUpRight size={16} aria-hidden="true" /></a></li>
          </ol>
          <p className="nightly-note">Vibeface is in Nightly. Updates can break compatibility before the first stable release. <a href="https://github.com/leaperone/vibeface/releases">View Nightly releases <ArrowUpRight size={14} aria-hidden="true" /></a></p>
        </section>
      </div>
    </SiteLayout>
  )
}
