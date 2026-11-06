import { spawn, spawnSync } from 'node:child_process'
import { join } from 'node:path'

const port = Number(process.env.PORT ?? 4399)
const baseUrl = process.env.PREVIEW_URL ?? `http://127.0.0.1:${port}`
const ownServer = !process.env.PREVIEW_URL

const pages = [
  { path: '/', title: 'tili — a tiling window manager for macOS' },
  { path: '/docs', title: 'Introduction — tili docs' },
  { path: '/docs/getting-started', title: 'Getting Started — tili docs' },
  { path: '/docs/configuration', title: 'Configuration — tili docs' },
  { path: '/docs/commands', title: 'Commands — tili docs' },
  { path: '/docs/menu-bar', title: 'Menu Bar Badge — tili docs' },
  { path: '/docs/architecture', title: 'Architecture — tili docs' },
  { path: '/docs/roadmap', title: 'Roadmap — tili docs' },
  { path: '/docs/changelog', title: 'Changelog — tili docs' },
]

const banned = [
  'Fullstack Engineer',
  'Claude Code',
  'Codex',
  'Ollama',
  'best tiling window manager',
  'no js',
  'best viewed',
]

const navigationHeaders = {
  Accept:
    'text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8',
  'Sec-Fetch-Dest': 'document',
  'Sec-Fetch-Mode': 'navigate',
  'Sec-Fetch-Site': 'none',
  'Upgrade-Insecure-Requests': '1',
}

const failures = []
const internalLinks = new Set()

function fail(message) {
  failures.push(message)
}

function get(path) {
  return fetch(`${baseUrl}${path}`, { headers: navigationHeaders })
}

function executableScripts(html) {
  const tags = html.match(/<script\b[^>]*>/gi) ?? []

  return tags.filter(
    (tag) =>
      !/type\s*=\s*["']application\/ld\+json["']/i.test(tag) &&
      !/data-enhancement=["']copy-(install|code)["']/i.test(tag) &&
      !/src=["']\/pagefind\/pagefind-component-ui\.js["']/i.test(tag),
  )
}

function parseJsonLd(html, path) {
  const types = []

  for (const match of html.matchAll(
    /<script[^>]*type=["']application\/ld\+json["'][^>]*>([\s\S]*?)<\/script>/gi,
  )) {
    try {
      const parsed = JSON.parse(match[1])
      const items = Array.isArray(parsed) ? parsed : [parsed]
      for (const item of items) types.push(item['@type'])
    } catch {
      fail(`${path}: invalid JSON-LD`)
    }
  }

  return types
}

function collectLinks(html) {
  for (const match of html.matchAll(/href="([^"]+)"/g)) {
    const href = match[1]
    if (!href.startsWith('/') || href.startsWith('//')) continue
    if (href.startsWith('/pagefind/')) continue
    internalLinks.add(href.split('#')[0])
  }
}

async function checkPage(page) {
  const response = await get(page.path)
  const html = await response.text()

  if (response.status !== 200) {
    fail(`${page.path}: expected 200, got ${response.status}`)
  }
  if (!html.includes(`<title>${page.title}</title>`)) {
    fail(`${page.path}: unexpected title`)
  }
  if ((html.match(/<h1\b/gi) ?? []).length !== 1) {
    fail(`${page.path}: expected exactly one h1`)
  }
  if (!/<meta[^>]+name=["']description["'][^>]*>/i.test(html)) {
    fail(`${page.path}: missing description`)
  }
  if (
    !html.includes(
      `<link rel="canonical" href="https://tili.dezen.me${page.path}">`,
    )
  ) {
    fail(`${page.path}: missing or wrong canonical`)
  }
  if (executableScripts(html).length > 0) {
    fail(`${page.path}: unexpected executable script`)
  }
  for (const text of banned) {
    if (html.includes(text)) fail(`${page.path}: banned text "${text}"`)
  }

  const types = parseJsonLd(html, page.path)
  if (!types.includes('WebSite')) fail(`${page.path}: missing WebSite JSON-LD`)

  if (page.path === '/') {
    for (const type of ['SoftwareApplication', 'FAQPage']) {
      if (!types.includes(type)) fail(`/: missing ${type} JSON-LD`)
    }
    for (const text of [
      'brew install itsdezen/tap/tili',
      'public Accessibility API',
      'Getting Started',
      'Does tili require disabling SIP?',
    ]) {
      if (!html.includes(text)) fail(`/: missing "${text}"`)
    }
    if (/pagefind-component-ui\.js/.test(html)) {
      fail('/: Pagefind must not load on the landing page')
    }
  } else {
    for (const type of ['TechArticle', 'BreadcrumbList']) {
      if (!types.includes(type)) fail(`${page.path}: missing ${type} JSON-LD`)
    }
    if (!/data-pagefind-body/.test(html)) {
      fail(`${page.path}: missing data-pagefind-body`)
    }
    if (!/<pagefind-modal[\s>]/.test(html)) {
      fail(`${page.path}: missing search modal`)
    }
    if (!/pagefind-component-ui\.js/.test(html)) {
      fail(`${page.path}: missing Pagefind bundle`)
    }
    const docLinks = (html.match(/href="\/docs[^"]*"/g) ?? []).length
    if (docLinks < 8) fail(`${page.path}: docs navigation looks incomplete`)
  }

  collectLinks(html)
}

async function checkNotFound() {
  const response = await get('/definitely-not-a-page')
  const html = await response.text()

  if (response.status !== 404) {
    fail(`/definitely-not-a-page: expected 404, got ${response.status}`)
  }
  if (!/noindex/i.test(html)) fail('404: missing noindex')
  if (!/<h1\b/.test(html)) fail('404: missing h1')
}

async function checkAssets() {
  for (const [path, type] of [
    ['/pagefind/pagefind-component-ui.js', 'javascript'],
    ['/pagefind/pagefind-component-ui.css', 'text/css'],
    ['/og-image.png', 'image/png'],
    ['/favicon.svg', 'image/svg+xml'],
    ['/site.webmanifest', 'application/manifest+json'],
  ]) {
    const response = await fetch(`${baseUrl}${path}`)
    const contentType = response.headers.get('content-type') ?? ''

    if (response.status !== 200) {
      fail(`${path}: expected 200, got ${response.status}`)
    }
    if (!contentType.includes(type)) {
      fail(`${path}: unexpected content type ${contentType}`)
    }
  }

  const manifest = await (await fetch(`${baseUrl}/site.webmanifest`)).json()
  if (manifest.theme_color !== '#f2ede1') {
    fail('/site.webmanifest: theme_color must stay ivory')
  }
}

async function checkSitemap() {
  const index = await fetch(`${baseUrl}/sitemap-index.xml`)
  const indexBody = await index.text()

  if (index.status !== 200) fail('/sitemap-index.xml: not served')
  if (!indexBody.includes('sitemap-0.xml')) {
    fail('/sitemap-index.xml: missing sitemap-0.xml')
  }

  const sitemap = await fetch(`${baseUrl}/sitemap-0.xml`)
  const body = await sitemap.text()

  if (sitemap.status !== 200) fail('/sitemap-0.xml: not served')
  for (const page of pages) {
    if (!body.includes(`https://tili.dezen.me${page.path}`)) {
      fail(`/sitemap-0.xml: missing ${page.path}`)
    }
  }
  if (body.includes('https://tili.dezen.me/404')) {
    fail('/sitemap-0.xml: must not list 404')
  }

  const robots = await (await fetch(`${baseUrl}/robots.txt`)).text()
  if (!robots.includes('Sitemap: https://tili.dezen.me/sitemap-index.xml')) {
    fail('/robots.txt: missing sitemap pointer')
  }
}

async function checkInternalLinks() {
  for (const href of internalLinks) {
    if (!href) continue
    const response = await get(href)
    if (response.status !== 200) {
      fail(`${href}: linked internally but returned ${response.status}`)
    }
  }
}

function startPreview() {
  const binary = join(process.cwd(), 'node_modules/.bin/astro')
  const child = spawn(
    binary,
    ['preview', '--port', String(port), '--host', '127.0.0.1'],
    { stdio: ['ignore', 'pipe', 'pipe'] },
  )

  child.stderr.on('data', (chunk) => process.stderr.write(chunk))

  return child
}

// Astro hands the server to a background process, so killing the spawned
// process is not enough: the lock file has to be cleared as well.
function stopPreview() {
  const binary = join(process.cwd(), 'node_modules/.bin/astro')
  spawnSync(binary, ['preview', 'stop'], { stdio: 'ignore' })
}

async function waitForServer(timeoutMs = 30_000) {
  const started = Date.now()

  while (Date.now() - started < timeoutMs) {
    try {
      const response = await fetch(`${baseUrl}/`)
      if (response.ok) return
    } catch {
      // Not listening yet.
    }
    await new Promise((resolve) => setTimeout(resolve, 250))
  }

  throw new Error(`preview did not start on ${baseUrl}`)
}

if (ownServer) stopPreview()

const server = ownServer ? startPreview() : undefined

try {
  if (server) await waitForServer()

  await checkSitemap()
  await checkAssets()
  for (const page of pages) await checkPage(page)
  await checkNotFound()
  await checkInternalLinks()
} catch (error) {
  fail(
    `harness crashed: ${error instanceof Error ? error.message : String(error)}`,
  )
} finally {
  server?.kill('SIGTERM')
  if (ownServer) stopPreview()
}

if (failures.length > 0) {
  console.error(`[harness] failed (${failures.length}):`)
  for (const failure of failures) console.error(`  - ${failure}`)
  process.exit(1)
}

console.log(
  `[harness] passed — ${pages.length} pages, 404, sitemap, robots, manifest, Pagefind assets, JSON-LD, internal links, and no-JS checks.`,
)
