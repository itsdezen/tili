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
    title: 'Event-driven',
    text: 'The daemon subscribes to Accessibility and workspace notifications and reacts to them, so it does no work while your windows are not changing.',
  },
  {
    title: 'Public Accessibility API only',
    text: 'Exactly one narrowly-scoped private call is used anywhere in the codebase, to resolve a window’s real CGWindowID. Everything else is the public Accessibility API, and you never disable SIP.',
  },
  {
    title: 'Session restore',
    text: 'Layout, focus, the active workspace per monitor, floating geometry, and special states — native fullscreen, minimized, hidden app — survive a restart or crash.',
  },
  {
    title: 'Native-tab guard',
    text: 'Opening, closing, or switching an app-owned tab never creates a placement or relayouts the workspace.',
  },
  {
    title: 'Display-aware workspaces',
    text: 'Every workspace has a home display. The workspace command routes there, workspace-here pulls it to the focused display, and move-workspace-to-monitor relocates it. Every connected display gets an active workspace, with temporary display workspaces cleaned up on disconnect.',
  },
  {
    title: 'Window rules and floating geometry',
    text: 'Match app-id, title, AX role, and subrole; route to a workspace; choose float, tile, or ignore; and size floating windows absolutely or by ratio with optional centering. Workspace rules route an app’s first window by default, with always for every window.',
  },
  {
    title: 'Opt-in animation',
    text: 'Moves are instant by default. Turn on animate for an eased transition with a configurable duration, including a new window’s entry into its slot.',
  },
  {
    title: 'Placement at window creation',
    text: 'A new window joins the layout the moment it appears, so placement does not wait for a later refresh.',
  },
  {
    title: 'Mouse drag-resize',
    text: 'Drag a tile’s edge or corner; the result snaps to the same grid as keyboard resizes.',
  },
  {
    title: 'KDL configuration, hot reloaded',
    text: 'Workspaces, keybinding modes, and floating rules are written in KDL and nest the way they actually relate. Save the file and the change is live, no daemon restart.',
  },
  {
    title: 'Menu bar workspace badge',
    text: 'tili-menubar ships with the daemon and shows the active workspace on the focused monitor, without a second status bar app.',
  },
  {
    title: 'Signed releases through Homebrew',
    text: 'The tap installs a real, signed tili.app. tili is MIT licensed and released through Homebrew.',
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
