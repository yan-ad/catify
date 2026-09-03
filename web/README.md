# Catify landing page

Static, SEO-friendly landing page built with [Vike](https://vike.dev/) and React.

```sh
bun install
bun run dev
```

Build the prerendered site:

```sh
bun run build
```

The deployable output is generated in `dist/client/`.

## Deploy to Vercel

The repository includes Vercel configuration for both supported project layouts:

- Repository root as the Vercel Root Directory: use the root `vercel.json`.
- `web` as the Vercel Root Directory: `web/vercel.json` is used automatically.

Both configurations publish the prerendered `dist/client/` output. Redeploy after
changing the Root Directory or build settings in the Vercel dashboard.
