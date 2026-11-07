import { defineConfig } from 'vitepress'
import generated from './generated/sidebar.json' with { type: 'json' }

// The reference groups for the HTTP API and the MCP tools are *generated*:
// `tidepool-docsgen` writes sidebar.json from the endpoint table and the tool
// list, so a new endpoint group or tool turns up here without an edit.
// Everything else is the prose, whose order is a judgement and is written.
const generatedGroup = (title: string, page: string, items: { text: string; link: string }[]) => ({
  text: title,
  collapsed: true,
  items: [{ text: 'overview', link: page }, ...items],
})

export default defineConfig({
  // Served as a project page, https://propell.dev/tidepool/: propell.dev is
  // the CNAME on the account's user site, so every asset url needs the
  // prefix. Wrong, the html loads and every stylesheet and script 404s.
  base: '/tidepool/',
  title: 'tidepool',
  description: 'tidepool is a live analytics server in one Rust binary. Push rows over HTTP, keep the model and the dashboards in git, and click to filter every number on the page.',
  lang: 'en',
  cleanUrls: true,
  // this directory's own readme is for whoever edits the site
  srcExclude: ['readme.md'],
  lastUpdated: true,
  // pages point at a local server (`http://localhost:7070/...`); every
  // other dead link still fails the build
  ignoreDeadLinks: 'localhostLinks',
  head: [
    // teal 9, the UI's accent fill. The type is the system's, as in the UI,
    // so nothing is fetched (see landing/visual-language.md).
    ['meta', { name: 'theme-color', content: '#12a594' }],
    // the UI's mark; `head` is not given the base, so it's written out
    ['link', { rel: 'icon', type: 'image/svg+xml', href: '/tidepool/favicon.svg' }],
    ['meta', { property: 'og:title', content: 'tidepool' }],
    ['meta', {
      property: 'og:description',
      content: 'Live analytics in one Rust binary. Rows over HTTP, the model and the dashboards in git, associative selections.',
    }],
  ],
  markdown: {
    theme: { light: 'github-light', dark: 'github-dark' },
  },
  themeConfig: {
    siteTitle: 'tidepool',
    outline: [2, 3],
    nav: [
      { text: 'guide', link: '/getting-started', activeMatch: '/(getting-started|project|data|operating|agents)' },
      { text: 'reference', link: '/reference/', activeMatch: '/reference/' },
      { text: 'contributing', link: '/contributing/testing', activeMatch: '/contributing/' },
      { text: 'kayak', link: 'https://propell.dev/kayak/' },
    ],
    sidebar: [
      {
        text: 'introduction',
        items: [
          { text: 'what tidepool is', link: '/' },
          { text: 'getting started', link: '/getting-started' },
        ],
      },
      {
        text: 'the project',
        items: [
          { text: 'the project directory', link: '/project/the-project' },
          { text: 'tables', link: '/project/tables' },
          { text: 'the model', link: '/project/the-model' },
          { text: 'metrics', link: '/project/metrics' },
          { text: 'hierarchies', link: '/project/hierarchies' },
          { text: 'dashboards', link: '/project/dashboards' },
          { text: 'filters and selections', link: '/project/selections' },
          { text: 'changes', link: '/project/changes' },
        ],
      },
      {
        text: 'data in and out',
        items: [
          { text: 'writing rows', link: '/data/writing-rows' },
          { text: 'queries', link: '/data/queries' },
          { text: 'the live protocol', link: '/data/live' },
          { text: 'kayak', link: '/data/kayak' },
        ],
      },
      {
        text: 'running a server',
        items: [
          { text: 'durability', link: '/operating/durability' },
          { text: 'access', link: '/operating/access' },
          { text: 'monitoring', link: '/operating/monitoring' },
          { text: 'deployment', link: '/operating/deployment' },
        ],
      },
      {
        text: 'agents',
        items: [{ text: 'mcp', link: '/agents' }],
      },
      {
        text: 'reference',
        items: [
          { text: 'how the reference is generated', link: '/reference/' },
          {
            text: 'config files',
            collapsed: false,
            items: [
              { text: 'tidepool.yaml', link: '/reference/config/server' },
              { text: 'tables/&lt;name&gt;.yaml', link: '/reference/config/table' },
              { text: 'model.ossie.yaml', link: '/reference/config/model' },
              { text: 'extensions', link: '/reference/config/extensions' },
              { text: 'dashboards/&lt;id&gt;.yaml', link: '/reference/config/dashboard' },
            ],
          },
          generatedGroup('http api', '/reference/api', generated.api),
          { text: 'schemas', link: '/reference/schemas' },
          generatedGroup('mcp tools', '/reference/mcp', generated.mcp),
          { text: 'command line', link: '/reference/cli' },
        ],
      },
      {
        text: 'contributing',
        items: [
          { text: 'testing', link: '/contributing/testing' },
          { text: 'benchmarks', link: '/contributing/benchmarks' },
          { text: 'the doc site', link: '/contributing/the-doc-site' },
        ],
      },
    ],
    socialLinks: [
      { icon: 'github', link: 'https://github.com/niclasgrahm/tidepool' },
    ],
    search: { provider: 'local' },
    editLink: {
      pattern: 'https://github.com/niclasgrahm/tidepool/edit/main/website/:path',
      text: 'edit this page',
    },
    footer: {
      message: 'reference generated from the schemas, the endpoint table and the MCP tools: <code>just docs</code>',
      copyright: 'tidepool',
    },
  },
})
