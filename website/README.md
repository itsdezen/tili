# tili website

Landing page (`/`) and documentation (`/docs`) for
[tili](https://github.com/itsdezen/tili). Astro 7, plain CSS, no client
framework, no dark mode, no animation.

## Local dev

```sh
bun install
bun run dev
```

Docs search is served by Pagefind, which indexes `dist/` after a build, so it
only works in `preview` (or production):

```sh
bun run build
bun run preview
```

## Build

```sh
bun run build
```

Runs `astro build`, then `pagefind --site dist`, then `scripts/check-build.mjs`
(SEO, canonical, one-`h1`, internal links, search assets, sitemap).

Outputs a static site to `dist/`.

## Quality gate

```sh
bun run verify   # check + lint + format:check + test + harness
```

`scripts/harness.mjs` starts `astro preview`, then checks every route, the 404
page, sitemap, robots, manifest, Pagefind assets, JSON-LD, and internal links
over HTTP.

## Social card

The OG image is generated from `src/assets/og-image.svg`:

```sh
bun run og
```

It loads Georgia/Verdana/Courier from the macOS system fonts, so run it locally
rather than in CI.

## Deploy

Cloudflare Workers static assets — root directory `website`, build command
`bun run build`, output directory `dist`. Target domain: `tili.dezen.me`.
`wrangler.jsonc` uses `drop-trailing-slash` so `/docs/commands` serves
`docs/commands.html`.
