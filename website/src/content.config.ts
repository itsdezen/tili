import { defineCollection } from 'astro:content'
import { glob } from 'astro/loaders'
import { z } from 'zod'

// Docs are plain Markdown: no MDX components are used anywhere, so there is no
// reason to pay for the MDX pipeline. The description doubles as the page's
// meta description, hence the length floor and ceiling.
const docs = defineCollection({
  loader: glob({ base: './src/content/docs', pattern: '**/*.md' }),
  schema: z.object({
    title: z.string().min(1),
    description: z.string().min(80).max(160),
  }),
})

export const collections = { docs }
