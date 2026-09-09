import { useState } from 'react'

const installCommand = 'npm install --global catify-cli'

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
    <main className="page">
      <section className="landing" aria-labelledby="page-title">
        <a className="wordmark" href="/" aria-label="Catify home">catify</a>

        <img
          className="hero-logo"
          src="/catify-logo.png"
          alt="Catify mascot: a black cat with a terminal and the Rust logo"
          width="1254"
          height="1254"
          fetchPriority="high"
        />

        <div className="intro">
          <h1 id="page-title">A Shopify CLI, written in Rust.</h1>
          <p>Independent, native, and familiar by design.</p>
        </div>

        <div className="install-command" aria-label="Install Catify with npm">
          <code>{installCommand}</code>
          <CopyButton />
        </div>

        <p className="platforms">macOS <span>·</span> Linux <span>·</span> Windows</p>

        <nav className="links" aria-label="Catify links">
          <a href="https://www.npmjs.com/package/catify-cli" target="_blank" rel="noreferrer">npm</a>
          <span>·</span>
          <a href="https://github.com/yan-ad/catify/blob/main/docs/installation.md" target="_blank" rel="noreferrer">installation</a>
          <span>·</span>
          <a href="https://github.com/yan-ad/catify/blob/main/inventory/CLI-PARITY.md" target="_blank" rel="noreferrer">compatibility</a>
          <span>·</span>
          <a href="https://github.com/yan-ad/catify" target="_blank" rel="noreferrer">GitHub</a>
        </nav>

        <p className="disclaimer">Experimental software. Not affiliated with Shopify.</p>
      </section>
    </main>
  )
}
