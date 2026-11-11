#!/usr/bin/env node
//
// Prints the GitHub release body for one version, derived from CHANGELOG.md.
//
// `release.yml` and `scripts/rewrite-releases.sh` both call this, so a release
// cut today and a release edited after the fact read exactly the same, and the
// body always points at the version's anchor on the website changelog.
//
//   node website/scripts/release-notes.mjs v0.15.1
//   node website/scripts/release-notes.mjs v0.15.1 --unsigned
//
// Exits non-zero when the changelog has no section for the version: a tag
// pushed without its changelog entry is a mistake worth failing on, not a
// release page that ships almost empty.

import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import {
  findSection,
  parseChangelog,
  renderReleaseNotes,
} from './changelog.mjs'

const args = process.argv.slice(2)
const tag = args.find((arg) => !arg.startsWith('--'))
const unsigned = args.includes('--unsigned')

if (!tag) {
  console.error(
    'usage: node website/scripts/release-notes.mjs <tag> [--unsigned]',
  )
  process.exit(2)
}

const version = tag.replace(/^v/, '')
const source = fileURLToPath(new URL('../../CHANGELOG.md', import.meta.url))
const { sections } = parseChangelog(readFileSync(source, 'utf-8'))
const section = findSection(sections, version)

if (!section) {
  console.error(
    `CHANGELOG.md has no '## [${version}]' section — add one before tagging`,
  )
  process.exit(1)
}

process.stdout.write(renderReleaseNotes({ section, signed: !unsigned }))
