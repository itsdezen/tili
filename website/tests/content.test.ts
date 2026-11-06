import { readdirSync, readFileSync } from 'node:fs'
import { join } from 'node:path'
import { describe, expect, it } from 'vitest'
import { docNeighbors, docOrder, docPath, sortDocs } from '../src/lib/docs'
import { faq, features, installSteps, site } from '../src/lib/site'

const docsDir = new URL('../src/content/docs/', import.meta.url).pathname

interface Doc {
  id: string
  title: string
  description: string
  body: string
}

function frontmatter(source: string): {
  data: Record<string, string>
  body: string
} {
  const match = source.match(/^---\n([\s\S]*?)\n---\n([\s\S]*)$/)

  if (!match) throw new Error('Missing frontmatter')

  const data: Record<string, string> = {}
  for (const line of match[1].split('\n')) {
    const separator = line.indexOf(':')
    if (separator === -1) continue
    data[line.slice(0, separator).trim()] = line.slice(separator + 1).trim()
  }

  return { data, body: match[2] }
}

const docs: Array<Doc> = readdirSync(docsDir)
  .filter((file) => file.endsWith('.md'))
  .map((file) => {
    const id = file.replace(/\.md$/, '')
    const { data, body } = frontmatter(
      readFileSync(join(docsDir, file), 'utf-8'),
    )

    return {
      id,
      title: data.title ?? '',
      description: data.description ?? '',
      body,
    }
  })

const paths = new Set(docs.map((doc) => docPath(doc.id)))

describe('docs content', () => {
  it('ships the eight documented pages', () => {
    expect(docs.map((doc) => doc.id).sort()).toEqual([...docOrder].sort())
  })

  it('has a unique, short title per page', () => {
    const titles = docs.map((doc) => doc.title)
    expect(new Set(titles).size).toBe(titles.length)

    for (const doc of docs) {
      expect(doc.title.length, doc.id).toBeGreaterThan(2)
      expect(doc.title.length, doc.id).toBeLessThanOrEqual(40)
    }
  })

  it('has a meta-length description per page', () => {
    for (const doc of docs) {
      expect(doc.description.length, doc.id).toBeGreaterThanOrEqual(80)
      expect(doc.description.length, doc.id).toBeLessThanOrEqual(160)
    }
    expect(new Set(docs.map((doc) => doc.description)).size).toBe(docs.length)
  })

  it('contains no Starlight-only syntax', () => {
    for (const doc of docs) {
      expect(doc.body, doc.id).not.toMatch(/^:::/m)
      expect(doc.body, doc.id).not.toMatch(/tableOfContents/)
    }
  })

  it('keeps every internal docs link resolvable', () => {
    for (const doc of docs) {
      for (const match of doc.body.matchAll(/\]\((\/docs[^)#]*)/g)) {
        expect(paths.has(match[1]), `${doc.id} -> ${match[1]}`).toBe(true)
      }
    }
  })

  it('keeps the callouts that replaced the admonitions', () => {
    const labels = docs.flatMap((doc) =>
      [...doc.body.matchAll(/^> \*\*(Note|Tip|Caution)\*\*$/gm)].map(
        (match) => `${doc.id}:${match[1]}`,
      ),
    )

    expect(labels.sort()).toEqual([
      'commands:Note',
      'configuration:Caution',
      'configuration:Note',
      'configuration:Note',
      'configuration:Note',
      'configuration:Note',
      'getting-started:Tip',
    ])
  })
})

describe('docs routing', () => {
  it('maps the index entry to /docs and everything else under it', () => {
    expect(docPath('index')).toBe('/docs')
    expect(docPath('commands')).toBe('/docs/commands')
  })

  it('sorts entries by the published order', () => {
    const sorted = sortDocs([
      { id: 'changelog' },
      { id: 'index' },
      { id: 'commands' },
    ])
    expect(sorted.map((entry) => entry.id)).toEqual([
      'index',
      'commands',
      'changelog',
    ])
  })

  it('reports neighbours for the pager', () => {
    const entries = sortDocs(docs.map((doc) => ({ id: doc.id, data: doc })))
    const { previous, next } = docNeighbors(entries, 'commands')

    expect(previous?.id).toBe('configuration')
    expect(next?.id).toBe('menu-bar')
  })
})

describe('site copy', () => {
  it('keeps the install command and the release links intact', () => {
    expect(site.installCommand).toBe('brew install itsdezen/tap/tili')
    expect(site.repository).toBe('https://github.com/itsdezen/tili')
    expect(installSteps).toHaveLength(2)
  })

  it('lists the tiling-window-manager features and FAQ', () => {
    expect(features).toHaveLength(7)
    expect(faq).toHaveLength(5)

    for (const item of [...features, ...faq]) {
      expect(Object.values(item).join(' ').length).toBeGreaterThan(20)
    }
  })

  it('never claims a benchmark, ranking, or private metric', () => {
    const copy = JSON.stringify({ site, features, faq }).toLowerCase()

    for (const banned of [
      'faster than',
      'best tiling window manager',
      'years of experience',
      'award',
    ]) {
      expect(copy).not.toContain(banned)
    }
  })
})
