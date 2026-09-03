import { useState } from 'react'

const installCommand = 'npm install --global catify-cli'

const stats = [
  { value: '73%', label: 'command compatibility' },
  { value: '24.7×', label: 'lower peak memory' },
  { value: '2.9×', label: 'smaller install size' },
  { value: '93.6×', label: 'faster warm startup' },
]

const features = [
  {
    number: '01',
    title: 'Native where it matters',
    body: 'Parsing, configuration, filesystem behavior, transport, and output are implemented directly in Rust—not hidden behind another CLI.',
  },
  {
    number: '02',
    title: 'Built for real workflows',
    body: 'Use familiar app and theme command structures while keeping predictable output, stable exit codes, and automation-friendly JSON.',
  },
  {
    number: '03',
    title: 'Open by default',
    body: 'MIT licensed, independently built, and developed in public with an explicit compatibility matrix and reproducible benchmarks.',
  },
]

function ArrowIcon() {
  return (
    <svg viewBox="0 0 16 16" aria-hidden="true">
      <path d="M3 8h9M8.5 3.5 13 8l-4.5 4.5" />
    </svg>
  )
}

function GithubIcon() {
  return (
    <svg viewBox="0 0 24 24" aria-hidden="true">
      <path d="M12 2C6.48 2 2 6.58 2 12.23c0 4.52 2.87 8.35 6.84 9.7.5.1.68-.22.68-.49l-.01-1.9c-2.78.62-3.37-1.2-3.37-1.2-.45-1.18-1.11-1.5-1.11-1.5-.91-.63.07-.62.07-.62 1 .08 1.53 1.06 1.53 1.06.9 1.56 2.35 1.11 2.92.85.1-.66.35-1.11.63-1.37-2.22-.26-4.55-1.14-4.55-5.06 0-1.12.39-2.03 1.03-2.75-.1-.26-.45-1.3.1-2.71 0 0 .84-.28 2.75 1.05A9.35 9.35 0 0 1 12 6.95a9.3 9.3 0 0 1 2.5.34c1.91-1.33 2.75-1.05 2.75-1.05.55 1.41.2 2.45.1 2.71.64.72 1.03 1.63 1.03 2.75 0 3.93-2.34 4.8-4.57 5.05.36.32.68.94.68 1.9l-.01 2.81c0 .27.18.59.69.49A10.24 10.24 0 0 0 22 12.23C22 6.58 17.52 2 12 2Z" />
    </svg>
  )
}

function CopyButton() {
  const [copied, setCopied] = useState(false)

  async function copyCommand() {
    try {
      await navigator.clipboard.writeText(installCommand)
      setCopied(true)
      window.setTimeout(() => setCopied(false), 1800)
    } catch {
      setCopied(false)
    }
  }

  return (
    <button className="copy-button" type="button" onClick={copyCommand}>
      <span className="copy-icon" aria-hidden="true">{copied ? '✓' : '⧉'}</span>
      {copied ? 'Copied' : 'Copy'}
    </button>
  )
}

export function Page() {
  return (
    <div className="site-shell">
      <header className="site-header">
        <a className="brand" href="#top" aria-label="Catify home">
          <span className="brand-mark" aria-hidden="true">c_</span>
          <span>catify</span>
        </a>
        <nav aria-label="Main navigation">
          <a href="#why">Why Catify</a>
          <a href="#benchmarks">Benchmarks</a>
          <a
            className="nav-github"
            href="https://github.com/yan-ad/catify"
            target="_blank"
            rel="noreferrer"
          >
            <GithubIcon />
            GitHub
          </a>
        </nav>
      </header>

      <main id="top">
        <section className="hero" aria-labelledby="hero-title">
          <div className="hero-copy">
            <p className="eyebrow reveal reveal-1">
              <span className="pulse" aria-hidden="true" />
              Experimental · open source · Rust native
            </p>
            <h1 id="hero-title" className="reveal reveal-2">
              Shopify workflows.
              <br />
              <em>Less waiting.</em>
            </h1>
            <p className="hero-lede reveal reveal-3">
              Catify is an independent, memory-efficient CLI built for developers who want familiar Shopify workflows without carrying a heavy runtime everywhere.
            </p>
            <div className="hero-actions reveal reveal-4">
              <a className="button button-primary" href="#install">
                Install Catify <ArrowIcon />
              </a>
              <a
                className="button button-secondary"
                href="https://github.com/yan-ad/catify"
                target="_blank"
                rel="noreferrer"
              >
                View source
              </a>
            </div>
          </div>

          <div className="hero-art reveal reveal-3" aria-hidden="true">
            <div className="art-frame">
              <span className="frame-label">CFY / NATIVE MODE</span>
              <img src="/og-image.svg" alt="" width="1200" height="630" />
              <span className="frame-index">001</span>
            </div>
          </div>

          <div id="benchmarks" className="stat-grid reveal reveal-4">
            {stats.map((stat) => (
              <div className="stat" key={stat.label}>
                <strong>{stat.value}</strong>
                <span>{stat.label}</span>
              </div>
            ))}
          </div>
          <p className="benchmark-note">
            Benchmarked on macOS arm64 against Shopify CLI 4.7.1. See the repository for methodology and current results.
          </p>
        </section>

        <section id="why" className="manifesto section-pad">
          <div className="section-heading">
            <p className="kicker">Why Catify</p>
            <h2>Small tool.<br />Serious intent.</h2>
          </div>
          <div className="feature-list">
            {features.map((feature) => (
              <article className="feature" key={feature.number}>
                <span>{feature.number}</span>
                <div>
                  <h3>{feature.title}</h3>
                  <p>{feature.body}</p>
                </div>
              </article>
            ))}
          </div>
        </section>

        <section id="install" className="install-section section-pad" aria-labelledby="install-title">
          <div className="install-copy">
            <p className="kicker">Start here</p>
            <h2 id="install-title">One command.<br />Native speed.</h2>
            <p>
              Install through npm on macOS, Linux, or Windows. Node.js handles the installation; your commands run in the native Rust binary.
            </p>
          </div>

          <div className="terminal" aria-label="Installation commands">
            <div className="terminal-bar">
              <span className="terminal-dots" aria-hidden="true"><i /><i /><i /></span>
              <span>~/your-project</span>
              <span>bash</span>
            </div>
            <div className="terminal-body">
              <div className="command-row">
                <code><span>$</span> {installCommand}</code>
                <CopyButton />
              </div>
              <code className="response">+ catify-cli@0.1.0</code>
              <code><span>$</span> cfy version</code>
              <code className="response">catify 0.1.0 <b>▮</b></code>
            </div>
          </div>
        </section>

        <section className="cta section-pad">
          <div>
            <p className="kicker">Built in public</p>
            <h2>A sharper CLI starts with an open conversation.</h2>
          </div>
          <a
            className="button button-invert"
            href="https://github.com/yan-ad/catify"
            target="_blank"
            rel="noreferrer"
          >
            Explore on GitHub <ArrowIcon />
          </a>
        </section>
      </main>

      <footer>
        <a className="brand footer-brand" href="#top">
          <span className="brand-mark" aria-hidden="true">c_</span>
          <span>catify</span>
        </a>
        <p>Independent and not affiliated with, endorsed by, or sponsored by Shopify.</p>
        <p>MIT licensed · Built with Rust</p>
      </footer>
    </div>
  )
}
