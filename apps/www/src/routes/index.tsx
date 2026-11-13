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
            <h1 id="hero-title">Your computers.<br />One conversation.</h1>
            <p className="hero-lede">Ask a bot to work on your Mac or Linux machine. Choose where it runs and read the result in the same chat.</p>
            <div className="hero-actions">
              <Link className="button button-primary" to="/docs/$" params={{ _splat: 'quick-start' }}>Start with one computer <ArrowRight size={16} aria-hidden="true" /></Link>
              <Link className="button button-quiet" to="/docs/$" params={{ _splat: 'overview' }}>Read the overview <ArrowRight size={16} aria-hidden="true" /></Link>
            </div>
          </div>

          <figure className="hero-figure" aria-label="A visual map of a conversation, the selected computer, and shared history">
            <div className="hero-visual">
              <div className="visual-backdrop" aria-hidden="true"><div className="visual-grid" /><div className="visual-glow" /></div>
              <div className="orbit orbit-one" aria-hidden="true" />
              <div className="orbit orbit-two" aria-hidden="true" />
              <div className="orbit orbit-three" aria-hidden="true" />
              <svg className="visual-path" viewBox="0 0 100 100" aria-hidden="true">
                <path id="signal-route" d="M 66 21 C 46 14 18 38 34 66 C 42 86 56 96 64 83 C 80 70 86 42 66 21 Z" />
                <circle className="visual-signal-glow" r="2.6"><animateMotion dur="9s" repeatCount="indefinite" begin="-0.3s"><mpath href="#signal-route" /></animateMotion></circle>
                <circle className="visual-signal" r="1.1"><animateMotion dur="9s" repeatCount="indefinite"><mpath href="#signal-route" /></animateMotion></circle>
              </svg>
              <div className="hero-logo"><img src="/logo.svg" width="136" height="136" alt="" /></div>
              <a className="visual-node visual-node-top" href="/docs/bot-and-execution"><span>Conversation</span><small>Shape the request, then hand it off</small></a>
              <a className="visual-node visual-node-left" href="/docs/bot-and-execution#工作如何回到原聊天"><span>Selected computer</span><small>Your Agent uses local tools</small></a>
              <a className="visual-node visual-node-right" href="/docs/history-sync"><span>Shared history</span><small>Stages and results return here</small></a>
            </div>
            <figcaption>From request to result, in one conversation.</figcaption>
          </figure>
        </section>

        <section className="landing-section landing-loop" aria-labelledby="loop-title">
          <div className="loop-intro">
            <h2 id="loop-title">The work stays together.</h2>
            <p className="section-copy">One request, one computer you choose, one thread that holds the result.</p>
            <p className="loop-prompt"><span>Example request</span>“Check the failing tests and tell me what to change.”</p>
          </div>
          <div className="loop-flow">
          <ol className="loop-steps" role="list">
            <li><span className="loop-num" aria-hidden="true">01</span><div><h3>Ask the bot</h3><p>Share the goal, project, and computer you want to use. The bot uses the thread to plan the work.</p></div></li>
            <li><span className="loop-num" aria-hidden="true">02</span><div><h3>Choose a computer</h3><p>An approved computer runs the work with its local Agent, files, and tools.</p></div></li>
            <li><span className="loop-num" aria-hidden="true">03</span><div><h3>Read the result</h3><p>Updates and saved results return to the same chat. If a result is unknown, check it before you retry.</p></div></li>
          </ol>
          <p className="loop-note">Live progress is not saved or synced.</p>
          </div>
        </section>

        <section className="landing-section landing-system" aria-labelledby="system-title">
          <h2 id="system-title" className="statement">Continue the conversation elsewhere. <em>Choose where the work runs.</em></h2>
          <dl className="fact-rows">
            <div><dt>Conversation and execution</dt><dd>Use a bot to coordinate work while Codex, Claude Code, pi, opencode, or Cursor runs it on the computer you choose.<br /><a className="text-link" href="/docs/bot-and-execution">See how work is split <ArrowRight size={16} aria-hidden="true" /></a></dd></div>
            <div><dt>Shared history</dt><dd>Approved devices share conversations, settings, task updates, and saved results. Project files, logins, and Agent sessions stay on each computer.<br /><a className="text-link" href="/docs/history-sync">See what syncs <ArrowRight size={16} aria-hidden="true" /></a></dd></div>
            <div><dt>Continue elsewhere</dt><dd>Open a synchronized thread on another computer and hand off the next turn. Replaying history never starts old work.</dd></div>
          </dl>
        </section>

        <section className="landing-section landing-choice" aria-labelledby="choice-title">
          <h2 id="choice-title">Already have an Agent?</h2>
          <p className="section-copy">Choose the tool that matches how you work.</p>
          <div className="choice-grid">
            <article className="choice-card choice-card-lead"><h3>Choose Vibeface</h3><p>You want one thread that can send work to your own computers and show the result back.</p><a className="text-link" href="/docs/comparison">Compare the options <ArrowRight size={16} aria-hidden="true" /></a></article>
            <article className="choice-card"><h3>Use Codex or Claude Code</h3><p>You only need the product's own terminal, editor, or hosted flow.</p></article>
            <article className="choice-card"><h3>Look at OpenClaw</h3><p>You want a self-hosted service that connects agents to chat apps.</p></article>
          </div>
        </section>

        <section className="landing-band landing-trust" aria-labelledby="trust-title">
          <div className="band-inner">
            <h2 id="trust-title">Know where your work goes.</h2>
            <div className="trust-grid">
              <dl className="trust-facts">
                <div><dt>Every device has its own identity</dt><dd>Each device holds a separate key. You approve new devices before they join.</dd></div>
                <div><dt>Connections are encrypted</dt><dd>A public entry point can relay encrypted traffic without joining your private network.</dd></div>
                <div><dt>Credentials stay local</dt><dd>AI service settings do not sync. A secret you send in chat does become part of the shared conversation.</dd></div>
              </dl>
              <aside className="trust-boundary" aria-label="Permission boundaries to know before you start">
                <p>Approved computers and the model service you choose handle your task content. Use devices and websites you trust.</p>
                <p>Encrypted connections do not encrypt files on disk. Agents can run commands and edit files on the host. Project folder settings are not an operating system sandbox.</p>
                <a className="text-link" href="/docs/privacy-security">Read privacy and permissions <ArrowRight size={16} aria-hidden="true" /></a>
              </aside>
            </div>
          </div>
        </section>

        <section className="landing-section landing-start" aria-labelledby="start-title">
          <h2 id="start-title">Start with one computer.</h2>
          <p className="section-copy">You need a supported Mac or Linux computer, one installed Agent, a model service, and a project folder. Start with one conversation, then add another computer.</p>
          <div className="hero-actions start-actions">
            <Link className="button button-primary" to="/docs/$" params={{ _splat: 'quick-start' }}>Open the quick start <ArrowRight size={16} aria-hidden="true" /></Link>
          </div>
          <ul className="start-links" role="list">
            <li><a className="text-link" href="/docs/onboarding">Set up a device <ArrowUpRight size={16} aria-hidden="true" /></a></li>
            <li><a className="text-link" href="/docs/user-guide">Read the user guide <ArrowUpRight size={16} aria-hidden="true" /></a></li>
          </ul>
          <p className="hourly-note">Vibeface ships as hourly builds. Updates can break compatibility before the first stable release. <a href="https://github.com/leaperone/vibeface/releases">View hourly releases</a></p>
        </section>
      </div>
    </SiteLayout>
  )
}
