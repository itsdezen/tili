// Copies each table header's text onto the matching body cell as a
// `data-label`. The docs tables are reference tables with 4–5 columns, which
// only fit a phone by scrolling sideways; with the labels, CSS can stack a row
// into a readable block instead.
function textContent(node) {
  if (typeof node.value === 'string') return node.value

  return (node.children ?? []).map(textContent).join('')
}

function labelTable(table) {
  const thead = (table.children ?? []).find(
    (child) => child.tagName === 'thead',
  )
  const headerRow = (thead?.children ?? []).find(
    (child) => child.tagName === 'tr',
  )

  if (!headerRow) return

  const labels = (headerRow.children ?? [])
    .filter((cell) => cell.tagName === 'th')
    .map((cell) => textContent(cell).trim())

  if (labels.length === 0) return

  const body = (table.children ?? []).find((child) => child.tagName === 'tbody')

  for (const row of body?.children ?? []) {
    if (row.tagName !== 'tr') continue

    const cells = (row.children ?? []).filter((cell) => cell.tagName === 'td')

    cells.forEach((cell, index) => {
      if (!labels[index]) return

      cell.properties = { ...cell.properties, dataLabel: labels[index] }
    })
  }
}

export default function rehypeTableLabels() {
  return (tree) => {
    const walk = (node) => {
      if (node.tagName === 'table') {
        labelTable(node)
        return
      }

      for (const child of node.children ?? []) walk(child)
    }

    walk(tree)
  }
}
