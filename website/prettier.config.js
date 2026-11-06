// @ts-check

/** @type {import('prettier').Config} */
const config = {
  semi: false,
  singleQuote: true,
  trailingComma: 'all',
  plugins: ['prettier-plugin-astro'],
  overrides: [{ files: '*.astro', options: { parser: 'astro' } }],
}

export default config
