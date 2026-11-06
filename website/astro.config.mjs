import { createReadStream, existsSync, statSync } from 'node:fs'
import { join, normalize, resolve, sep } from 'node:path'
import sitemap from '@astrojs/sitemap'
import { defineConfig } from 'astro/config'

const contentTypes = {
  '.css': 'text/css; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.json': 'application/json; charset=utf-8',
  '.pagefind': 'application/octet-stream',
  '.wasm': 'application/wasm',
}

// Pagefind writes its bundle into `dist/` after the build, so the dev server
// has nothing to serve for /pagefind/*. Mirror the built bundle in dev when it
// exists, which keeps docs search usable without a separate preview server.
function pagefindDevServer() {
  return {
    name: 'pagefind-dev-server',
    hooks: {
      'astro:server:setup': ({ server }) => {
        const root = resolve(process.cwd(), 'dist/pagefind')

        server.middlewares.use((request, response, next) => {
          const url = (request.url ?? '').split('?')[0]

          if (!url.startsWith('/pagefind/')) return next()

          const file = join(root, normalize(url.slice('/pagefind/'.length)))

          if (
            !file.startsWith(root + sep) ||
            !existsSync(file) ||
            !statSync(file).isFile()
          ) {
            return next()
          }

          response.setHeader(
            'Content-Type',
            contentTypes[url.slice(url.lastIndexOf('.'))] ??
              'application/octet-stream',
          )
          createReadStream(file).pipe(response)
        })
      },
    },
  }
}

// Fully static site: the landing page and every docs page are prerendered at
// build time. Pagefind indexes `dist/` afterwards and ships its own bundle, so
// no adapter and no server runtime are involved.
export default defineConfig({
  site: 'https://tili.dezen.me',
  output: 'static',
  integrations: [
    sitemap({
      // The 404 page is served but never indexed.
      filter: (page) => !page.endsWith('/404'),
      namespaces: {
        news: false,
        xhtml: false,
        image: false,
        video: false,
      },
    }),
    pagefindDevServer(),
  ],
  markdown: {
    // Light-only site: a light syntax theme keeps code blocks readable on the
    // ivory surface without a second color scheme.
    shikiConfig: { theme: 'github-light' },
  },
  build: {
    // `file` emits /docs/commands.html so Cloudflare's `drop-trailing-slash`
    // handling serves /docs/commands, matching the canonical URLs.
    format: 'file',
    inlineStylesheets: 'auto',
  },
  trailingSlash: 'never',
  compressHTML: true,
  devToolbar: { enabled: false },
  prefetch: false,
})
