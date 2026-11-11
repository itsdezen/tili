import { readdirSync, readFileSync } from 'node:fs'
import { join } from 'node:path'
import { describe, expect, it } from 'vitest'
import {
  EXPANDED_RELEASES,
  INSTALL_COMMAND,
  REPOSITORY_URL,
  SITE_URL,
  countChanges,
  extractHighlights,
  findSection,
  parseChangelog,
  renderReleaseNotes,
  renderSitePage,
  splitSubsections,
  topLevelBullets,
  versionAnchor,
} from '../scripts/changelog.mjs'
import { site } from '../src/lib/site'

const repoRoot = new URL('../../', import.meta.url).pathname
const docsDir = new URL('../src/content/docs/', import.meta.url).pathname

const markdown = readFileSync(join(repoRoot, 'CHANGELOG.md'), 'utf-8')
const committedPage = readFileSync(join(docsDir, 'changelog.md'), 'utf-8')
const { intro, sections } = parseChangelog(markdown)

const docs = readdirSync(docsDir)
  .filter((file) => file.endsWith('.md'))
  .map((file) => readFileSync(join(docsDir, file), 'utf-8'))

describe('changelog parsing', () => {
  it('reads every release, newest first, with no duplicates', () => {
    expect(sections.length).toBeGreaterThan(0)
    expect(new Set(sections.map((section) => section.version)).size).toBe(
      sections.length,
    )

    const rank = (version: string) =>
      version.split('.').reduce((total, part) => total * 1000 + Number(part), 0)

    for (let index = 1; index < sections.length; index += 1) {
      expect(rank(sections[index - 1].version)).toBeGreaterThan(
        rank(sections[index].version),
      )
    }
  })

  it('gives every release a Keep a Changelog heading', () => {
    for (const section of sections) {
      expect(section.date).toMatch(/^\d{4}-\d{2}-\d{2}$/)
      expect(countChanges(section), section.version).toBeGreaterThan(0)
    }
  })

  it('drops the duplicated top-level title', () => {
    expect(intro).not.toMatch(/^# /m)
    expect(intro).toContain('Keep a Changelog')
  })
})

describe('changelog page', () => {
  it('matches what the generator produces', () => {
    expect(committedPage).toBe(renderSitePage({ intro, sections }))
  })

  it('anchors every release by version, not by date', () => {
    for (const version of sections.map((section) => section.version)) {
      expect(committedPage).toContain(
        `<span id="${versionAnchor(version)}"></span>`,
      )
    }

    expect(versionAnchor('0.15.1')).toBe('v0.15.1')
  })

  it('collapses the older releases behind a summary', () => {
    expect(committedPage).toContain('<details>')
    expect(committedPage).toContain(
      `<summary>Earlier releases (v${sections[sections.length - 1].version} – v${
        sections[EXPANDED_RELEASES].version
      })</summary>`,
    )
    expect(committedPage).not.toMatch(/^# /m)
  })

  it('shares its constants with the site copy', () => {
    expect(SITE_URL).toBe(site.url)
    expect(REPOSITORY_URL).toBe(site.repository)
    expect(INSTALL_COMMAND).toBe(site.installCommand)
  })

  it('resolves every changelog anchor the docs link to', () => {
    const versions = new Set(sections.map((section) => section.version))
    const linked = docs.flatMap((doc) => [
      ...doc.matchAll(/\/docs\/changelog#v(\d+\.\d+\.\d+)/g),
    ])

    expect(linked.length).toBeGreaterThan(0)
    for (const match of linked) {
      expect(versions.has(match[1]), match[0]).toBe(true)
    }
  })
})

describe('release notes', () => {
  it('leads with install, a highlight or two, and a link to the full entry', () => {
    for (const section of sections.slice(0, 5)) {
      const body = renderReleaseNotes({ section, signed: true })

      expect(body).toContain(`**Install:** \`${INSTALL_COMMAND}\``)
      expect(body).toContain(
        `**Full changelog → ${SITE_URL}/docs/changelog#${versionAnchor(section.version)}**`,
      )
      expect(body).not.toContain('Unsigned development build')
      expect(body).not.toMatch(/…and 0 more/)
      expect(countChanges(section)).toBeGreaterThanOrEqual(
        (body.match(/^- /gm) ?? []).length,
      )
    }
  })

  it('always warns when the build is unsigned', () => {
    const body = renderReleaseNotes({ section: sections[0], signed: false })

    expect(body).toContain('⚠️ **Unsigned development build.**')
    expect(body).toContain('xattr -d com.apple.quarantine tili.app')
  })

  it('puts a migration note ahead of the feature list', () => {
    const section = sections.find((candidate) =>
      splitSubsections(candidate).groups.some(
        (group) => group.title === 'Migration',
      ),
    )

    expect(section).toBeDefined()

    const migration = splitSubsections(section!).groups.find(
      (group) => group.title === 'Migration',
    )
    const firstMigration = topLevelBullets(migration!.text)[0]

    expect(extractHighlights(section!).bullets[0]).toContain(
      firstMigration.replace(/^\*\*/, '').slice(0, 40),
    )
  })

  it('reports a version the changelog does not have', () => {
    expect(findSection(sections, '99.99.99')).toBeUndefined()
  })
})
