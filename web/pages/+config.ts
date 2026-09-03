import type { Config } from 'vike/types'
import vikeReact from 'vike-react/config'

export default {
  extends: [vikeReact],
  prerender: true,
  title: 'Catify — A fast, Rust-native Shopify CLI alternative',
  description:
    'Catify is an independent, memory-efficient Shopify CLI alternative built natively in Rust for fast app and theme development workflows.',
  favicon: '/favicon.png',
  lang: 'en',
  viewport: 'responsive',
} satisfies Config
