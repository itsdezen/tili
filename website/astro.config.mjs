import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

export default defineConfig({
  site: 'https://tili.dezen.me',
  integrations: [
    starlight({
      title: 'tili',
      description: 'A tiling window manager for macOS, built for speed.',
      // `replacesTitle: true` keeps the site title as the link's accessible
      // name (rendered sr-only) but hides it visually, so the nav shows just
      // the logo mark — same logo-only treatment as the landing page hero.
      //
      // `light`/`dark` (rather than a single `src`) is what makes the logo
      // follow Starlight's own theme toggle instead of just system
      // preference: an externally-referenced <img>'s SVG can only react to
      // `prefers-color-scheme` internally, which would drift from a manual
      // toggle — Starlight instead renders both images and shows/hides them
      // itself via its `light:sl-hidden`/`dark:sl-hidden` classes, which do
      // track its own toggle state. Same path data as public/favicon.svg
      // (which *does* use prefers-color-scheme, correctly, since a browser
      // tab's favicon has no page toggle to follow), just pre-split into two
      // flat-fill files instead of one self-swapping one.
      logo: {
        light: './src/assets/logo-light.svg',
        dark: './src/assets/logo-dark.svg',
        alt: 'tili',
        replacesTitle: true,
      },
      social: [{ icon: 'github', label: 'GitHub', href: 'https://github.com/itsdezen/tili' }],
      customCss: ['./src/styles/starlight-overrides.css'],
      // Starlight's own `favicon` option only produces one <link rel="shortcut
      // icon">, so it's pointed at the legacy .ico fallback here — the
      // preferred SVG (dark/light aware, see public/favicon.svg) and the
      // other RealFaviconGenerator-produced fallbacks are added below via
      // `head` instead, same set as the landing page's own <head>.
      favicon: '/favicon.ico',
      head: [
        { tag: 'link', attrs: { rel: 'icon', type: 'image/svg+xml', href: '/favicon.svg' } },
        // Safari-specific: the pinned-tab/Touch Bar silhouette, a wholly
        // different mechanism from the regular favicon above — Safari
        // uses this file purely as a monochrome alpha mask and tints it
        // with `color` itself, ignoring whatever fill the file has.
        { tag: 'link', attrs: { rel: 'mask-icon', href: '/favicon-mask.svg', color: '#171717' } },
        {
          tag: 'link',
          attrs: { rel: 'icon', type: 'image/png', sizes: '96x96', href: '/favicon-96x96.png' },
        },
        {
          tag: 'link',
          attrs: { rel: 'apple-touch-icon', sizes: '180x180', href: '/apple-touch-icon.png' },
        },
        { tag: 'link', attrs: { rel: 'manifest', href: '/site.webmanifest' } },
        {
          tag: 'meta',
          attrs: { name: 'theme-color', content: '#0a0a0a', media: '(prefers-color-scheme: light)' },
        },
        {
          tag: 'meta',
          attrs: { name: 'theme-color', content: '#ffffff', media: '(prefers-color-scheme: dark)' },
        },
        // Docs content (and the GitHub icon in the nav) are the only
        // places on the site with a variable, content-authored set of
        // external links, so this runs once per page load rather than
        // hand-adding target/rel to every markdown link — the landing
        // page's own (fixed, small) set of external links is instead
        // hand-written directly in src/pages/index.astro.
        {
          tag: 'script',
          content: `(function () {
            function markExternalLinks() {
              document.querySelectorAll('a[href^="http"]').forEach(function (a) {
                if (a.hostname === location.hostname) return;
                a.target = '_blank';
                var rel = (a.getAttribute('rel') || '').split(/\\s+/).filter(Boolean);
                if (rel.indexOf('noopener') === -1) rel.push('noopener');
                if (rel.indexOf('noreferrer') === -1) rel.push('noreferrer');
                a.setAttribute('rel', rel.join(' '));
              });
            }
            document.addEventListener('DOMContentLoaded', markExternalLinks);
          })();`,
        },
      ],
      sidebar: [
        { label: 'Introduction', slug: 'docs' },
        { label: 'Getting Started', slug: 'docs/getting-started' },
        { label: 'Configuration', slug: 'docs/configuration' },
        { label: 'Commands', slug: 'docs/commands' },
        { label: 'Menu Bar Badge', slug: 'docs/menu-bar' },
        { label: 'Architecture', slug: 'docs/architecture' },
        { label: 'Roadmap', slug: 'docs/roadmap' },
        { label: 'Changelog', slug: 'docs/changelog' },
      ],
    }),
  ],
});
