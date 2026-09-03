import { useState } from 'react'

const installCommand = 'npm install --global catify-cli'

const stats = [
  ['73%', 'command compatibility'],
  ['24.7×', 'lower peak memory'],
  ['2.9×', 'smaller install'],
  ['93.6×', 'faster warm start'],
]

const principles = [
  {
    title: 'Native by default',
    body: 'Command parsing, configuration, filesystem behavior, transport, and output are implemented directly in Rust—not delegated to another CLI.',
  },
  {
    title: 'Familiar command surface',
    body: 'Catify follows Shopify CLI command names and nesting so existing knowledge and automation carry over with less friction.',
  },
  {
    title: 'Built in public',
    body: 'Compatibility status, benchmarks, architecture decisions, and known gaps are documented in the repository.',
  },
]

function CopyButton() {
  const [copied, setCopied] = useState(false)

  async function copyCommand() {
    try {
      await navigator.clipboard.writeText(installCommand)
      setCopied(true)
      window.setTimeout(() => setCopied(false), 1600)
    } catch {
      setCopied(false)
    }
  }

  return (
    <button type="button" className="copy-button" onClick={copyCommand}>
      {copied ? 'copied' : 'copy'}
    </button>
  )
}

export function Page() {
  return (
    <main className="page" id="top">
      <header className="masthead">
        <a className="wordmark" href="#top" aria-label="Catify home">catify</a>
        <nav aria-label="Main navigation">
          <a href="#about">About</a>
          <a href="#benchmarks">Benchmarks</a>
          <a href="https://github.com/yan-ad/catify" target="_blank" rel="noreferrer">GitHub</a>
        </nav>
      </header>

      <section className="hero" aria-labelledby="hero-title">
        <div className="hero-copy">
          <p className="status"><span aria-hidden="true" /> Experimental open source software</p>
          <h1 id="hero-title">A Shopify CLI,<br />written in Rust.</h1>
          <p className="lede">
            Catify is an independent, memory-efficient implementation of familiar Shopify developer workflows.
          </p>

          <div className="install-command" aria-label="Install Catify with npm">
            <code>{installCommand}</code>
            <CopyButton />
          </div>

          <p className="link-row">
            <a href="https://www.npmjs.com/package/catify-cli" target="_blank" rel="noreferrer">npm</a>
            <span>·</span>
            <a href="https://github.com/yan-ad/catify/blob/main/docs/installation.md" target="_blank" rel="noreferrer">installation</a>
            <span>·</span>
            <a href="https://github.com/yan-ad/catify" target="_blank" rel="noreferrer">source</a>
          </p>
        </div>

        <img
          className="hero-logo"
          src="/catify-logo.png"
          alt="Catify mascot: a black cat with a terminal and the Rust logo"
          width="1254"
          height="1254"
          fetchPriority="high"
        />
      </section>

      <section className="benchmarks" id="benchmarks" aria-labelledby="benchmark-title">
        <div className="section-heading">
          <p className="section-number">01</p>
          <h2 id="benchmark-title">Current snapshot</h2>
        </div>
        <div>
          <dl className="stat-list">
            {stats.map(([value, label]) => (
              <div key={label}>
                <dt>{label}</dt>
                <dd>{value}</dd>
              </div>
            ))}
          </dl>
          <p className="fine-print">
            Measured on macOS arm64 against Shopify CLI 4.7.1. Compatibility is 81 of 111 pinned commands. Memory is peak RSS; startup is median warm load time.
          </p>
          <p className="text-link">
            <a href="https://github.com/yan-ad/catify/blob/main/inventory/CLI-PARITY.md" target="_blank" rel="noreferrer">See compatibility matrix →</a>
          </p>
        </div>
      </section>

      <section className="about" id="about" aria-labelledby="about-title">
        <div className="section-heading">
          <p className="section-number">02</p>
          <h2 id="about-title">Why it exists</h2>
        </div>
        <div className="principles">
          {principles.map((principle) => (
            <article key={principle.title}>
              <h3>{principle.title}</h3>
              <p>{principle.body}</p>
            </article>
          ))}
        </div>
      </section>

      <section className="closing" aria-labelledby="closing-title">
        <h2 id="closing-title">Small binary.<br />Visible tradeoffs.</h2>
        <p>
          Catify is experimental and not yet a complete replacement. Follow the work, report gaps, or contribute on GitHub.
        </p>
        <a className="primary-link" href="https://github.com/yan-ad/catify" target="_blank" rel="noreferrer">
          View the repository →
        </a>
      </section>

      <footer>
        <p>MIT licensed · Built with Rust</p>
        <p>Not affiliated with, endorsed by, or sponsored by Shopify.</p>
      </footer>
    </main>
  )
}
