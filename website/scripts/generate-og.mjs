import { existsSync } from 'node:fs'
import { readFile, writeFile } from 'node:fs/promises'
import { Resvg } from '@resvg/resvg-js'

// The social card is authored as SVG and rendered to a 1200×630 PNG. System
// type is loaded explicitly so the output matches the site's Verdana / Andale
// Mono stack instead of whatever fallback font the renderer picks.
const fontDir = '/System/Library/Fonts/Supplemental'
const fontFiles = ['Verdana.ttf', 'Verdana Bold.ttf', 'Andale Mono.ttf']
  .map((file) => `${fontDir}/${file}`)
  .filter((file) => existsSync(file))

const source = await readFile(
  new URL('../src/assets/og-image.svg', import.meta.url),
)
const target = new URL('../public/og-image.png', import.meta.url)

const resvg = new Resvg(source, {
  fitTo: { mode: 'width', value: 1200 },
  font: {
    fontFiles,
    loadSystemFonts: fontFiles.length === 0,
    defaultFontFamily: 'Verdana',
  },
})

const png = resvg.render().asPng()
await writeFile(target, png)

console.log(`[og] wrote public/og-image.png (${png.length} bytes)`)
