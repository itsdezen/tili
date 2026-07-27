# tili website

Marketing landing page (`/`) + docs (`/docs`, Starlight) for
[tili](https://github.com/itsdezen/tili).

## Local dev

```sh
bun install
bun run dev
```

## Build

```sh
bun run build
```

Outputs a static site to `dist/`.

## Deploy

Cloudflare Pages — root directory `website`, build command `bun run build`,
output directory `dist`. Target domain: `tili.dezen.me`.
