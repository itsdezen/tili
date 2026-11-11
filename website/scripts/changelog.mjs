// Single source for how a CHANGELOG.md release section is rendered. The
// website's changelog page and every GitHub release body are both derived
// from the repository changelog, so they have to agree on what a release
// looks like — this module is the only place that decides.
//
// Everything here is a pure function over strings: no filesystem access, so
// the site generator, the release-notes CLI, and the tests all share it.
//
// SITE_URL / REPOSITORY_URL / INSTALL_COMMAND duplicate `src/lib/site.ts`
// because this module is also loaded by plain `node` in the release workflow,
// which can't import TypeScript. `tests/changelog.test.ts` asserts they still
// match, so the copy can't drift.

export const SITE_URL = 'https://tili.dezen.me'
export const REPOSITORY_URL = 'https://github.com/itsdezen/tili'
export const INSTALL_COMMAND = 'brew install itsdezen/tap/tili'

export const CHANGELOG_URL = `${REPOSITORY_URL}/blob/main/CHANGELOG.md`
export const SIGNING_DETAILS_URL = `${REPOSITORY_URL}/blob/main/CONTRIBUTING.md#release-engineering`

// Recent releases stay expanded; everything older collapses into one
// `<details>` block so the page doesn't open with a wall of old versions.
export const EXPANDED_RELEASES = 6

const VERSION_HEADING = /^## \[(\d+\.\d+\.\d+)\](?: - (\d{4}-\d{2}-\d{2}))?$/
const ANY_RELEASE_HEADING = /^## \[/
const UNRELEASED_HEADING = /^## \[unreleased\]$/

// Migration first: for someone deciding whether to upgrade it outranks the
// feature list, and it's the one section that can't be skipped. `Notes` is
// deliberately absent — implementation trivia ("needs full Xcode") belongs on
// the changelog page, not in a release summary.
const HIGHLIGHT_ORDER = ['Migration', 'Added', 'Fixed', 'Changed']
const HIGHLIGHT_LIMIT = 2
const HIGHLIGHT_MAX_CHARS = 240

const SIGNING_NOTE =
  'Direct downloads are self-signed, not notarized; use right-click → Open.' +
  ` [Signing details](${SIGNING_DETAILS_URL}).`

const UNSIGNED_NOTE =
  '⚠️ **Unsigned development build.** tili does not codesign or notarize ' +
  "release binaries yet — the self-signed certificate this project's release " +
  'strategy depends on ' +
  `(see [CONTRIBUTING.md](${SIGNING_DETAILS_URL}))` +
  " hasn't been generated. Installing via `brew install itsdezen/tap/tili` is " +
  "unaffected by any of this (Homebrew's `curl` download doesn't set macOS's " +
  'quarantine flag). If you download `tili.app` directly from this page ' +
  'instead, Gatekeeper will block it on first launch; run ' +
  '`xattr -d com.apple.quarantine tili.app` or right-click → Open to proceed. ' +
  'Not recommended for daily use before v1.0.'

const PAGE_DESCRIPTION =
  "The release history for tili, mirrored from the repository's CHANGELOG.md " +
  'and grouped by version with migration notes.'

/** The stable in-page anchor for a release: `/docs/changelog#v0.15.1`. */
export function versionAnchor(version) {
  return `v${version}`
}

/**
 * Splits CHANGELOG.md into its preamble and one entry per released version,
 * newest first, exactly as the file orders them.
 *
 * @param {string} markdown
 * @returns {{ intro: string, sections: Array<{ version: string, date: string, body: string }> }}
 */
export function parseChangelog(markdown) {
  const lines = markdown
    .replace(/\r\n?/g, '\n')
    .replace(/^\uFEFF/, '')
    .split('\n')
  const sections = []
  const intro = []

  let current = null
  let skipping = false

  for (const line of lines) {
    if (UNRELEASED_HEADING.test(line.trim().toLowerCase())) {
      current = null
      skipping = true
      continue
    }

    const match = VERSION_HEADING.exec(line.trim())

    if (match) {
      current = { version: match[1], date: match[2] ?? '', body: [] }
      sections.push(current)
      skipping = false
      continue
    }

    // A `[Unreleased]` heading is expected mid-cycle; any other bracketed
    // `##` heading means the file grew a shape neither the site nor the
    // release body knows how to render, so fail here instead of shipping a
    // page that silently drops it.
    if (ANY_RELEASE_HEADING.test(line)) {
      throw new Error(
        `CHANGELOG.md has an unrecognized release heading: ${line.trim()}`,
      )
    }

    if (skipping) continue
    if (current) current.body.push(line)
    else intro.push(line)
  }

  if (sections.length === 0) {
    throw new Error('CHANGELOG.md has no `## [x.y.z] - YYYY-MM-DD` sections')
  }

  const introLines = trimBlankEdges(intro)

  // Every docs page renders its own `<h1>` from frontmatter, so the
  // changelog's `# Changelog` title would be a second one.
  if (introLines[0]?.startsWith('# ')) introLines.shift()

  return {
    intro: trimBlankEdges(introLines).join('\n'),
    sections: sections.map((section) => ({
      version: section.version,
      date: section.date,
      body: trimBlankEdges(section.body).join('\n'),
    })),
  }
}

/** Finds one version's section; `undefined` when the changelog has no such release. */
export function findSection(sections, version) {
  return sections.find((section) => section.version === version)
}

/** Splits a section body into its intro text and its `### Heading` groups. */
export function splitSubsections(section) {
  const preamble = []
  const groups = []
  let current = null

  for (const line of section.body.split('\n')) {
    const match = /^### (.+)$/.exec(line)

    if (match) {
      current = { title: match[1].trim(), text: [] }
      groups.push(current)
      continue
    }

    if (current) current.text.push(line)
    else preamble.push(line)
  }

  return {
    preamble: trimBlankEdges(preamble).join('\n'),
    groups: groups.map((group) => ({
      title: group.title,
      text: trimBlankEdges(group.text).join('\n'),
    })),
  }
}

/** Number of top-level change bullets in a section. */
export function countChanges(section) {
  const { groups } = splitSubsections(section)

  return groups.reduce(
    (total, group) => total + topLevelBullets(group.text).length,
    0,
  )
}

/**
 * Picks the few lines a release body shows above its link to the full page:
 * one bullet from each of the highest-signal sections, in upgrade-decision
 * order. `more` is how many bullets that leaves behind on the changelog page.
 */
export function extractHighlights(section) {
  const { preamble, groups } = splitSubsections(section)
  const total = countChanges(section)
  const ordered = [...groups].sort(
    (left, right) => highlightRank(left.title) - highlightRank(right.title),
  )

  const bullets = []
  let primary = null

  for (const group of ordered) {
    if (bullets.length === HIGHLIGHT_LIMIT) break
    if (highlightRank(group.title) === HIGHLIGHT_ORDER.length) continue

    const groupBullets = topLevelBullets(group.text)
    if (groupBullets.length === 0) continue

    if (!primary) primary = groupBullets
    bullets.push(shortenBullet(groupBullets[0]))
  }

  // A release that keeps every change under one heading would otherwise get a
  // one-line summary; its next bullet is still the best signal available.
  if (bullets.length < HIGHLIGHT_LIMIT && primary) {
    for (const extra of primary.slice(
      1,
      HIGHLIGHT_LIMIT - bullets.length + 1,
    )) {
      bullets.push(shortenBullet(extra))
    }
  }

  return {
    preamble,
    bullets,
    more: Math.max(0, total - bullets.length),
  }
}

/** The GitHub release body for one version. */
export function renderReleaseNotes({ section, signed }) {
  const { preamble, bullets, more } = extractHighlights(section)
  const parts = [
    `**Install:** \`${INSTALL_COMMAND}\``,
    signed ? SIGNING_NOTE : UNSIGNED_NOTE,
  ]

  if (preamble) parts.push(preamble)
  if (bullets.length > 0)
    parts.push(bullets.map((line) => `- ${line}`).join('\n'))
  if (more > 0) {
    parts.push(`…and ${more} more ${more === 1 ? 'change' : 'changes'}.`)
  }
  parts.push(
    `**Full changelog → ${SITE_URL}/docs/changelog#${versionAnchor(section.version)}**`,
  )

  return `${parts.join('\n\n')}\n`
}

/**
 * The whole website changelog page, frontmatter included. Each release gets a
 * `<span id>` anchor rather than relying on the heading's auto slug: the slug
 * bakes the release date into the id (`#0150---2026-11-09`), so it would break
 * every link the moment a date is corrected. `span` matches the pattern
 * `commands.md` already uses for `cmd-*` anchors.
 */
export function renderSitePage({ intro, sections }) {
  const expanded = sections.slice(0, EXPANDED_RELEASES)
  const collapsed = sections.slice(EXPANDED_RELEASES)
  const parts = [
    '---',
    'title: Changelog',
    `description: ${PAGE_DESCRIPTION}`,
    '---',
    '',
    intro,
    '',
    introNote(),
    '',
    renderSections(expanded),
  ]

  if (collapsed.length > 0) {
    const oldest = collapsed[collapsed.length - 1].version
    const newest = collapsed[0].version

    parts.push(
      '',
      '<details>',
      `<summary>Earlier releases (v${oldest} – v${newest})</summary>`,
      '',
      renderSections(collapsed),
      '',
      '</details>',
    )
  }

  return `${parts
    .join('\n')
    .replace(/\n{3,}/g, '\n\n')
    .trimEnd()}\n`
}

function introNote() {
  return [
    'For planned work rather than history, see the [Roadmap](/docs/roadmap).',
    '',
    'Both this page and each GitHub release note are generated from the',
    `repository's [\`CHANGELOG.md\`](${CHANGELOG_URL}) — edit the changelog, not`,
    'this page.',
  ].join('\n')
}

function renderSections(sections) {
  return sections
    .map((section) => {
      const anchor = `<span id="${versionAnchor(section.version)}"></span>`
      const heading = `## [${section.version}] - ${section.date}`

      return [anchor, '', heading, '', section.body].join('\n')
    })
    .join('\n\n')
}

function highlightRank(title) {
  const index = HIGHLIGHT_ORDER.indexOf(title)

  return index === -1 ? HIGHLIGHT_ORDER.length : index
}

/**
 * A section's top-level bullets as single lines: the changelog hard-wraps at
 * 80 columns, and a highlight has to be the whole sentence rather than
 * whatever fit on its first physical line.
 */
export function topLevelBullets(text) {
  const bullets = []
  let current = null

  for (const line of text.split('\n')) {
    if (line.startsWith('- ')) {
      if (current !== null) bullets.push(current)
      current = line.slice(2).trim()
      continue
    }

    if (current === null) continue

    // A nested bullet belongs to the parent's detail, not to its summary, so
    // it ends the summary instead of being glued onto it.
    if (line.trim() === '' || line.trimStart().startsWith('- ')) {
      bullets.push(current)
      current = null
      continue
    }

    current = `${current} ${line.trim()}`
  }

  if (current !== null) bullets.push(current)

  return bullets
}

// The lead `**…**` of a bullet is the author's own one-line summary, so it
// reads better than a truncation of the paragraph after it. Bullets without
// one fall back to their first sentence.
function shortenBullet(text) {
  const bold = /^\*\*(.+?)\*\*/.exec(text)

  if (bold) return `**${truncate(bold[1])}**`

  return truncate(firstSentence(text))
}

function firstSentence(text) {
  const match = /^(.+?[.!?])(?:\s|$)/.exec(text)

  return match ? match[1] : text
}

function truncate(text) {
  if (text.length <= HIGHLIGHT_MAX_CHARS) return text

  const cut = text.slice(0, HIGHLIGHT_MAX_CHARS)
  const lastSpace = cut.lastIndexOf(' ')

  return `${cut.slice(0, lastSpace === -1 ? HIGHLIGHT_MAX_CHARS : lastSpace)}…`
}

function trimBlankEdges(lines) {
  let start = 0
  let end = lines.length

  while (start < end && lines[start].trim() === '') start += 1
  while (end > start && lines[end - 1].trim() === '') end -= 1

  return lines.slice(start, end)
}
