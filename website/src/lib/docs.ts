export const docOrder = [
  'index',
  'getting-started',
  'configuration',
  'commands',
  'menu-bar',
  'architecture',
  'roadmap',
  'changelog',
] as const

export type DocId = (typeof docOrder)[number]

export interface DocEntry {
  id: string
  data: {
    title: string
    description: string
  }
}

export function docPath(id: string): string {
  return id === 'index' ? '/docs' : `/docs/${id}`
}

export function sortDocs<T extends { id: string }>(
  entries: Array<T>,
): Array<T> {
  const rank = new Map<string, number>(docOrder.map((id, index) => [id, index]))

  return [...entries].sort((a, b) => {
    const left = rank.get(a.id) ?? Number.MAX_SAFE_INTEGER
    const right = rank.get(b.id) ?? Number.MAX_SAFE_INTEGER

    return left === right ? a.id.localeCompare(b.id) : left - right
  })
}

export function docNeighbors(
  entries: Array<DocEntry>,
  id: string,
): { previous?: DocEntry; next?: DocEntry } {
  const index = entries.findIndex((entry) => entry.id === id)

  if (index === -1) return {}

  return { previous: entries[index - 1], next: entries[index + 1] }
}
