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

## Changelog page

`src/content/docs/changelog.md` is **generated** from the repository's
`CHANGELOG.md` — never edit it by hand:

```sh
bun run sync:changelog           # rewrite the page
bun run sync:changelog --check   # fail if it is stale
```

`scripts/changelog.mjs` is the single renderer for both this page and the
GitHub release bodies: every release gets a `<span id="vX.Y.Z">` anchor, so
`https://tili.dezen.me/docs/changelog#v0.15.1` is a stable link that survives
edits to a release date or title. Recent releases stay expanded, the rest
collapse into one `<details>`; a small inline enhancement opens it when an
incoming hash points inside. `tests/changelog.test.ts` compares the committed
page against the generator's output, so drift fails the build instead of
shipping.

The same module drives `scripts/release-notes.mjs <tag>`, which `release.yml`
and `scripts/rewrite-releases.sh` use to render a release body.

## Quality gate

```sh
bun run verify   # check + lint + format:check + test + harness
```

`scripts/harness.mjs` starts `astro preview`, then checks every route, the 404
page, sitemap, robots, manifest, Pagefind assets, JSON-LD, and internal links
over HTTP.

CI runs the same command as the `website` job in `.github/workflows/ci.yml`.

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

Merging to `main` deploys automatically, so nothing here runs a deploy step —
but GitHub release bodies link to this site's changelog anchors, which makes a
finished deploy a prerequisite for `scripts/rewrite-releases.sh --apply` (it
checks the live page first).
