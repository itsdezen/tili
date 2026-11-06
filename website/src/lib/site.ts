export const site = {
  name: 'tili',
  url: 'https://tili.dezen.me',
  title: 'tili — a tiling window manager for macOS',
  description:
    'tili is an i3-style tiling window manager for macOS, built for speed: event-driven, public Accessibility API only, Rust, and no SIP disable.',
  repository: 'https://github.com/itsdezen/tili',
  releases: 'https://github.com/itsdezen/tili/releases',
  sponsor: 'https://github.com/sponsors/itsdezen',
  author: 'Dezen',
  authorUrl: 'https://dezen.me',
  installCommand: 'brew install itsdezen/tap/tili',
  ogImage: '/og-image.png',
  themeColor: '#f2ede1',
} as const

export interface NavLink {
  label: string
  href: string
  external?: boolean
}

export const navLinks: Array<NavLink> = [
  { label: 'Docs', href: '/docs' },
  { label: 'GitHub', href: site.repository, external: true },
  { label: 'Sponsor', href: site.sponsor, external: true },
]

export interface Feature {
  title: string
  text: string
}

export const features: Array<Feature> = [
  {
    title: 'Event-driven, not polling',
    text: 'The daemon subscribes to Accessibility and workspace notifications and reacts to them, so it does nothing while your windows are not changing.',
  },
  {
    title: 'Public Accessibility API only',
    text: 'Exactly one narrowly-scoped private call is used anywhere in the codebase, to resolve a window’s real CGWindowID. Everything else is the public Accessibility API, and you never disable SIP.',
  },
  {
    title: 'KDL configuration, hot reloaded',
    text: 'Workspaces, keybinding modes, and floating rules are written in KDL and nest the way they actually relate. Save the file and the change is live, no daemon restart.',
  },
  {
    title: 'Session restore',
    text: 'Layout, focus, the active workspace per monitor, floating geometry, and special states survive a daemon restart or a crash.',
  },
  {
    title: 'Display-aware workspaces',
    text: 'Every workspace has a home display. Route one with workspace, pull it to the focused display with workspace-here, or relocate it with move-workspace-to-monitor.',
  },
  {
    title: 'Menu bar workspace badge',
    text: 'tili-menubar ships with the daemon and shows the active workspace on the focused monitor, without a second status bar app.',
  },
  {
    title: 'Signed releases through Homebrew',
    text: 'The tap installs a real, signed tili.app. tili is MIT licensed and never needs the App Store sandbox to move your windows.',
  },
]

export interface FaqItem {
  question: string
  answer: string
}

export const faq: Array<FaqItem> = [
  {
    question: 'Does tili require disabling SIP?',
    answer:
      'No. tili runs on the public Accessibility API and never asks you to disable System Integrity Protection. Exactly one narrowly-scoped private call is used, to resolve a window’s real CGWindowID.',
  },
  {
    question: 'Is tili free and open source?',
    answer:
      'Yes. tili is MIT licensed, the source is on GitHub, and it installs through Homebrew, so there is no App Store sandbox between the window manager and your windows.',
  },
  {
    question: 'Does tili work with multiple monitors?',
    answer:
      'Yes. Every workspace has a home display, workspace-here pulls a workspace to the focused display, move-workspace-to-monitor relocates it, and inactive workspaces park on the southeast-most display.',
  },
  {
    question: 'How do I install tili on macOS?',
    answer:
      'Run brew install itsdezen/tap/tili, then tili start. The tap installs a signed tili.app, and tili start registers a background LaunchAgent that restarts with the system.',
  },
  {
    question: 'What permissions does tili need?',
    answer:
      'Accessibility and Input Monitoring, granted to tili.app on first run. tili waits for both grants and reports when it is running, so there is no restart in between.',
  },
]

export interface InstallStep {
  command: string
  text: string
}

export const installSteps: Array<InstallStep> = [
  {
    command: 'brew install itsdezen/tap/tili',
    text: 'Install the signed app from the Homebrew tap.',
  },
  {
    command: 'tili start',
    text: 'Install the LaunchAgent and start the daemon.',
  },
]
