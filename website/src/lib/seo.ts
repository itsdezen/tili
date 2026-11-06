import { faq, features, site } from './site'

export type JsonLd = Record<string, unknown>

export const seo = {
  siteUrl: site.url,
  siteName: site.name,
  title: site.title,
  description: site.description,
  defaultImage: site.ogImage,
  themeColor: site.themeColor,
} as const

export function absoluteUrl(path: string): string {
  return new URL(path, site.url).href
}

export function pageTitle(title: string): string {
  if (title === site.title) return site.title

  return `${title} — tili docs`
}

export function authorJsonLd(): JsonLd {
  return {
    '@type': 'Person',
    '@id': `${site.url}/#author`,
    name: site.author,
    url: site.authorUrl,
  }
}

export function websiteJsonLd(): JsonLd {
  return {
    '@context': 'https://schema.org',
    '@type': 'WebSite',
    '@id': `${site.url}/#website`,
    name: site.name,
    url: site.url,
    description: site.description,
    inLanguage: 'en',
    publisher: authorJsonLd(),
  }
}

export function softwareApplicationJsonLd(): JsonLd {
  return {
    '@context': 'https://schema.org',
    '@type': 'SoftwareApplication',
    name: site.name,
    description: site.description,
    url: site.url,
    applicationCategory: 'DeveloperApplication',
    operatingSystem: 'macOS',
    isAccessibleForFree: true,
    license: 'https://github.com/itsdezen/tili/blob/main/LICENSE',
    downloadUrl: site.releases,
    softwareHelp: absoluteUrl('/docs'),
    author: authorJsonLd(),
    sameAs: [site.repository, site.releases],
    offers: {
      '@type': 'Offer',
      price: '0',
      priceCurrency: 'USD',
    },
    featureList: features.map((feature) => feature.title),
  }
}

export function faqJsonLd(): JsonLd {
  return {
    '@context': 'https://schema.org',
    '@type': 'FAQPage',
    mainEntity: faq.map((item) => ({
      '@type': 'Question',
      name: item.question,
      acceptedAnswer: {
        '@type': 'Answer',
        text: item.answer,
      },
    })),
  }
}

export function techArticleJsonLd(input: {
  title: string
  description: string
  path: string
}): JsonLd {
  const url = absoluteUrl(input.path)

  return {
    '@context': 'https://schema.org',
    '@type': 'TechArticle',
    headline: input.title,
    description: input.description,
    url,
    mainEntityOfPage: url,
    inLanguage: 'en',
    author: authorJsonLd(),
    publisher: authorJsonLd(),
    isPartOf: { '@id': `${site.url}/#website` },
  }
}

export function breadcrumbJsonLd(
  items: Array<{ name: string; path: string }>,
): JsonLd {
  return {
    '@context': 'https://schema.org',
    '@type': 'BreadcrumbList',
    itemListElement: items.map((item, index) => ({
      '@type': 'ListItem',
      position: index + 1,
      name: item.name,
      item: absoluteUrl(item.path),
    })),
  }
}
