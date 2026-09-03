const softwareSchema = {
  '@context': 'https://schema.org',
  '@type': 'SoftwareApplication',
  name: 'Catify',
  alternateName: 'cfy',
  description:
    'An independent, memory-efficient Shopify CLI alternative built natively in Rust.',
  applicationCategory: 'DeveloperApplication',
  operatingSystem: 'macOS, Linux, Windows',
  softwareVersion: '0.1.0',
  license: 'https://opensource.org/license/mit',
  codeRepository: 'https://github.com/yan-ad/catify',
  downloadUrl: 'https://www.npmjs.com/package/catify-cli',
  offers: {
    '@type': 'Offer',
    price: '0',
    priceCurrency: 'USD',
  },
}

export function Head() {
  return (
    <>
      <meta
        name="keywords"
        content="Shopify CLI alternative, Rust CLI, Shopify theme development, Shopify app development, open source developer tools"
      />
      <meta name="robots" content="index, follow, max-image-preview:large" />
      <meta name="theme-color" content="#11120f" />
      <meta property="og:type" content="website" />
      <meta property="og:site_name" content="Catify" />
      <meta property="og:image" content="/og-image.png" />
      <meta property="og:image:type" content="image/png" />
      <meta property="og:image:width" content="1200" />
      <meta property="og:image:height" content="630" />
      <meta
        property="og:image:alt"
        content="Catify, a fast Rust-native Shopify CLI alternative"
      />
      <meta name="twitter:card" content="summary_large_image" />
      <meta name="twitter:image" content="/og-image.png" />
      <link rel="manifest" href="/site.webmanifest" />
      <link rel="preconnect" href="https://github.com" />
      <script
        type="application/ld+json"
        dangerouslySetInnerHTML={{ __html: JSON.stringify(softwareSchema) }}
      />
    </>
  )
}
