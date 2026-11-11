#!/usr/bin/env node
//
// Regenerates `src/content/docs/changelog.md` from the repository's
// CHANGELOG.md.
//
// The page is committed rather than gitignored: Cloudflare builds the site
// with a plain `bun run build`, and `tests/changelog.test.ts` re-derives the
// page and fails when the committed copy is stale — so the only way to get it
// wrong is to skip this script, which fails loudly instead of serving a page
// that quietly lags the changelog.
//
//   bun run sync:changelog          rewrite the page
//   bun run sync:changelog --check  exit non-zero when it's stale, no write

import { readFileSync, writeFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { parseChangelog, renderSitePage } from './changelog.mjs'

const source = fileURLToPath(new URL('../../CHANGELOG.md', import.meta.url))
const target = fileURLToPath(
  new URL('../src/content/docs/changelog.md', import.meta.url),
)

const { intro, sections } = parseChangelog(readFileSync(source, 'utf-8'))
const page = renderSitePage({ intro, sections })

if (process.argv.includes('--check')) {
  if (readFileSync(target, 'utf-8') !== page) {
    console.error(
      '[sync-changelog] src/content/docs/changelog.md is stale — run `bun run sync:changelog`',
    )
    process.exit(1)
  }

  console.log('[sync-changelog] up to date')
  process.exit(0)
}

writeFileSync(target, page)
console.log(
  `[sync-changelog] wrote src/content/docs/changelog.md (${sections.length} releases)`,
)
