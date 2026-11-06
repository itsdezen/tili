import { existsSync, readdirSync, readFileSync } from 'node:fs'
import { join, relative } from 'node:path'
import { fileURLToPath } from 'node:url'

const dist = fileURLToPath(new URL('../dist/', import.meta.url))
const errors = []
const htmlFiles = []
const jsFiles = []

// The only client-side JavaScript this site allows: the two inline
// progressive enhancements (copy install command, copy code block) and
// Pagefind's own bundle on docs pages. Everything else must stay static.
const enhancementScript =
  /data-enhancement=["'](copy-install|copy-code|heading-anchors)["']/
const pagefindScript =
  /<script[^>]+src=["']\/pagefind\/pagefind-component-ui\.js["'][^>]*>/i

function walk(dir) {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const fullPath = join(dir, entry.name)

    if (entry.isDirectory()) {
      walk(fullPath)
      continue
    }

    if (entry.name.endsWith('.html')) htmlFiles.push(fullPath)
    if (
      entry.name.endsWith('.js') &&
      relative(dist, fullPath).startsWith('_astro/')
    ) {
      jsFiles.push(fullPath)
    }
  }
}

function fail(message) {
  errors.push(message)
}

function localTargetExists(path) {
  const clean = decodeURIComponent(path.split('?')[0].split('#')[0])

  if (!clean.startsWith('/')) return true
  if (clean.startsWith('/pagefind/')) return true

  const base = clean.replace(/\/$/, '')
  const candidates =
    base === ''
      ? [join(dist, 'index.html')]
      : [
          join(dist, base),
          join(dist, `${base}.html`),
          join(dist, base, 'index.html'),
        ]

  return candidates.some((candidate) => existsSync(candidate))
}

if (!existsSync(dist)) {
  console.error('[check-build] dist/ not found. Run astro build first.')
  process.exit(1)
}

walk(dist)

if (htmlFiles.length === 0) fail('No HTML files found in dist/')
if (jsFiles.length > 0) {
  fail(
    `Bundled client JS emitted: ${jsFiles.map((file) => relative(dist, file)).join(', ')}`,
  )
}

// Copy that would be inaccurate or stale: version numbers pinned in the page,
// tool names that should never appear in public copy, and unverifiable claims.
const banned = [
  /Fullstack Engineer\b/i,
  /\bClaude Code\b/i,
  /\bCodex\b/i,
  /\bOllama\b/i,
  /\b\d+\+?\s*(years?|yoe)\b/i,
  /\d+(\.\d+)*\s*(x|×)\s*faster\b/i,
  /best (tiling )?window manager\b/i,
  /no js/i,
  /best viewed/i,
]

for (const file of htmlFiles) {
  const name = relative(dist, file)
  const html = readFileSync(file, 'utf-8')
  const isNotFound = name === '404.html'
  const isDocs = name.startsWith('docs')

  const scriptTags = html.match(/<script\b[^>]*>/gi) ?? []
  const executableScripts = scriptTags.filter(
    (tag) => !/type\s*=\s*["']application\/ld\+json["']/i.test(tag),
  )

  for (const tag of executableScripts) {
    const allowed = enhancementScript.test(tag) || pagefindScript.test(tag)

    if (!allowed) fail(`${name}: unexpected executable <script> ${tag}`)
  }

  if (isDocs) {
    if (!pagefindScript.test(html)) fail(`${name}: missing Pagefind bundle`)
    if (!/<pagefind-modal[\s>]/i.test(html))
      fail(`${name}: missing search modal`)
    if (!/data-pagefind-body/i.test(html)) fail(`${name}: missing search body`)
  } else if (/pagefind-component-ui\.js/i.test(html)) {
    fail(`${name}: Pagefind loaded outside docs`)
  }

  if (/<astro-island\b/i.test(html)) fail(`${name}: hydrated island found`)
  if (/\son[a-z]+\s*=/i.test(html)) fail(`${name}: inline event handler found`)
  if (/javascript:/i.test(html)) fail(`${name}: javascript: URL found`)

  for (const pattern of banned) {
    if (pattern.test(html)) fail(`${name}: banned text ${pattern}`)
  }

  const titleMatch = html.match(/<title>([^<]+)<\/title>/i)
  if (!titleMatch) {
    fail(`${name}: missing <title>`)
  } else if (titleMatch[1].length > 65) {
    fail(`${name}: title too long (${titleMatch[1].length})`)
  }

  if (!/<meta[^>]+name=["']description["'][^>]*>/i.test(html)) {
    fail(`${name}: missing meta description`)
  }

  const h1Count = (html.match(/<h1\b/gi) ?? []).length
  if (h1Count !== 1) fail(`${name}: expected one <h1>, got ${h1Count}`)

  const isNoIndex = /name=["']robots["'][^>]*content=["'][^"']*noindex/i.test(
    html,
  )
  if (!isNoIndex && !/<link[^>]+rel=["']canonical["'][^>]*>/i.test(html)) {
    fail(`${name}: missing canonical`)
  }

  if (isNotFound && !isNoIndex) fail(`${name}: 404 page must be noindex`)

  for (const match of html.matchAll(/(href|src)="([^"]+)"/g)) {
    const target = match[2]

    if (
      !target ||
      target.startsWith('#') ||
      target.startsWith('mailto:') ||
      target.startsWith('tel:') ||
      target.startsWith('data:') ||
      target.startsWith('javascript:') ||
      target.startsWith('http:') ||
      target.startsWith('https:') ||
      target.startsWith('//')
    ) {
      continue
    }

    if (target.startsWith('/') && !localTargetExists(target)) {
      fail(`${name}: broken internal link ${target}`)
    }
  }
}

for (const required of [
  'index.html',
  '404.html',
  'docs.html',
  'docs/getting-started.html',
  'docs/configuration.html',
  'docs/commands.html',
  'docs/menu-bar.html',
  'docs/architecture.html',
  'docs/roadmap.html',
  'docs/changelog.html',
  'sitemap-index.xml',
  'sitemap-0.xml',
  'robots.txt',
  'site.webmanifest',
  'og-image.png',
  'pagefind/pagefind-component-ui.js',
  'pagefind/pagefind-component-ui.css',
]) {
  if (!existsSync(join(dist, required))) fail(`Missing dist/${required}`)
}

const sitemapPath = join(dist, 'sitemap-0.xml')
if (existsSync(sitemapPath)) {
  const sitemap = readFileSync(sitemapPath, 'utf-8')
  for (const requiredUrl of [
    'https://tili.dezen.me/',
    'https://tili.dezen.me/docs',
    'https://tili.dezen.me/docs/getting-started',
    'https://tili.dezen.me/docs/configuration',
    'https://tili.dezen.me/docs/commands',
    'https://tili.dezen.me/docs/menu-bar',
    'https://tili.dezen.me/docs/architecture',
    'https://tili.dezen.me/docs/roadmap',
    'https://tili.dezen.me/docs/changelog',
  ]) {
    if (!sitemap.includes(requiredUrl)) fail(`Sitemap missing ${requiredUrl}`)
  }
  if (sitemap.includes('https://tili.dezen.me/404')) {
    fail('Sitemap must not include the 404 page')
  }
}

if (errors.length > 0) {
  console.error('[check-build] failed:')
  for (const error of errors) console.error(`  - ${error}`)
  process.exit(1)
}

console.log(
  `[check-build] passed — ${htmlFiles.length} HTML pages, SEO/canonical/h1, internal links, search assets, sitemap verified.`,
)
