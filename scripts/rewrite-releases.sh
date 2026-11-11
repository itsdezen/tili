#!/usr/bin/env bash
#
# Rewrites the GitHub release bodies of already-published tags to the current
# format: install and signing lines, a couple of highlight bullets, and a link
# to the release's anchor on the website changelog.
#
# Every body comes from `website/scripts/release-notes.mjs`, the same generator
# `release.yml` uses for new tags, so history and new releases can't drift into
# two different shapes.
#
# Default is a dry run: it writes each proposed body to
# `<backup>/<tag>.new.md`, the current body to `<backup>/<tag>.old.md`, and
# prints one line per release. Nothing on GitHub changes until `--apply`.
#
#   scripts/rewrite-releases.sh                    # dry run, all tags
#   scripts/rewrite-releases.sh --only v0.15.1     # dry run, one tag
#   scripts/rewrite-releases.sh --apply            # write every rewrite
#   scripts/rewrite-releases.sh --restore <dir>    # put the saved bodies back
#
# The bodies link to `https://tili.dezen.me/docs/changelog#vX.Y.Z`, so this
# refuses to apply while any of those anchors is missing from the live page.
# `--skip-site-check` exists for the case where the site is known-good but
# unreachable from here.
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd "${SCRIPT_DIR}/.." && pwd)
SITE="https://tili.dezen.me"
PAGE="${SITE}/docs/changelog"
BACKUP_ROOT="${TMPDIR:-/tmp}"
BACKUP_DIR="${TILI_RELEASE_BACKUP:-${BACKUP_ROOT%/}/tili-release-backup-$(date +%Y%m%d-%H%M%S)}"

APPLY=0
SKIP_SITE_CHECK=0
ONLY=""
RESTORE_DIR=""

die() {
  echo "rewrite-releases: $*" >&2
  exit 1
}

usage() {
  sed -n '2,26p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

while [ $# -gt 0 ]; do
  case "$1" in
    --apply) APPLY=1 ;;
    --only)
      shift
      [ $# -gt 0 ] || die "--only needs a tag, e.g. --only v0.15.1"
      ONLY="$1"
      ;;
    --restore)
      shift
      [ $# -gt 0 ] || die "--restore needs the backup directory"
      RESTORE_DIR="$1"
      ;;
    --skip-site-check) SKIP_SITE_CHECK=1 ;;
    -h | --help)
      usage
      exit 0
      ;;
    *) die "unknown argument: $1" ;;
  esac
  shift
done

command -v gh >/dev/null || die "gh is required"
gh auth status >/dev/null 2>&1 || die "gh is not authenticated"

if [ -n "$RESTORE_DIR" ]; then
  [ -d "$RESTORE_DIR" ] || die "no such backup directory: $RESTORE_DIR"

  for old in "$RESTORE_DIR"/*.old.md; do
    [ -e "$old" ] || die "no *.old.md bodies in $RESTORE_DIR"
    tag=$(basename "$old" .old.md)
    gh release edit "$tag" --notes-file "$old" >/dev/null
    echo "restored ${tag}"
  done
  exit 0
fi

tags=()
while IFS= read -r tag; do
  tags+=("$tag")
done < <(git -C "$REPO_ROOT" tag --list 'v*' | sort -V)

[ ${#tags[@]} -gt 0 ] || die "no v* tags in $REPO_ROOT"

if [ -n "$ONLY" ]; then
  found=0
  for tag in "${tags[@]}"; do
    [ "$tag" = "$ONLY" ] && found=1
  done
  [ "$found" -eq 1 ] || die "no such tag: $ONLY"
  tags=("$ONLY")
fi

if [ "$APPLY" -eq 1 ] && [ "$SKIP_SITE_CHECK" -eq 0 ]; then
  echo "checking ${PAGE} for every release anchor"
  page_html=$(curl -fsS "${PAGE}?v=${RANDOM}${RANDOM}") ||
    die "could not fetch ${PAGE}"

  missing=0
  for tag in "${tags[@]}"; do
    if ! printf '%s' "$page_html" | grep -q "id=\"v${tag#v}\""; then
      echo "  missing anchor: #v${tag#v}" >&2
      missing=$((missing + 1))
    fi
  done

  [ "$missing" -eq 0 ] ||
    die "${missing} anchor(s) missing — deploy the website first, or pass --skip-site-check"
fi

mkdir -p "$BACKUP_DIR"
echo "bodies: ${BACKUP_DIR}"

changed=0
for tag in "${tags[@]}"; do
  old="${BACKUP_DIR}/${tag}.old.md"
  new="${BACKUP_DIR}/${tag}.new.md"

  if [ ! -e "$old" ]; then
    gh release view "$tag" --json body -q .body >"$old" || die "no release for ${tag}"
  fi

  unsigned=""
  if grep -q "Unsigned development build" "$old"; then
    unsigned="--unsigned"
  fi

  # shellcheck disable=SC2086 # `unsigned` is either empty or one flag.
  node "${REPO_ROOT}/website/scripts/release-notes.mjs" "$tag" ${unsigned} >"$new" ||
    die "could not render ${tag} (is its CHANGELOG.md section present?)"

  if cmp -s "$old" "$new"; then
    echo "${tag}: unchanged"
    continue
  fi

  changes=$(grep -c '^- ' "$new" || true)
  echo "${tag}: ${changes} highlight(s), $(wc -l <"$new" | tr -d ' ') lines -> #v${tag#v}"

  if [ "$APPLY" -eq 1 ]; then
    gh release edit "$tag" --notes-file "$new" >/dev/null
    changed=$((changed + 1))
  fi
done

if [ "$APPLY" -eq 1 ]; then
  echo "updated ${changed} release(s)"
  echo "undo: scripts/rewrite-releases.sh --restore ${BACKUP_DIR}"
else
  echo "dry run — review ${BACKUP_DIR}/*.new.md, then re-run with --apply"
fi
