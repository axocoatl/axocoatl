// @ts-check
import { defineConfig } from 'astro/config';
import { unified } from '@astrojs/markdown-remark';
import starlight from '@astrojs/starlight';
import { emailOff } from './scripts/email-off.mjs';

// https://astro.build/config
export default defineConfig({
  site: 'https://docs.axocoatl.ai',
  redirects: {
    '/getting-started': '/start/install/',
    '/guides/ollama-quickstart': '/operate/verification/',
    '/guides/architecture': '/understand/architecture/',
    '/guides/providers': '/configure/providers/',
    '/guides/token-budgets': '/configure/agents/',
    '/guides/tool-approval': '/configure/skills-mcp/',
    '/guides/troubleshooting': '/operate/troubleshooting/',
    '/guides/examples-gallery': '/reference/examples/',
    '/concepts/sessions': '/understand/product-model/',
    '/concepts/agents': '/configure/agents/',
    '/concepts/coordinator': '/understand/coordination/',
    '/concepts/skills': '/configure/skills-mcp/',
    '/concepts/memory': '/understand/state/',
    '/concepts/checkpointing': '/understand/state/',
    '/concepts/automations': '/configure/automations/',
    '/concepts/lattice': '/understand/coordination/',
    '/concepts/protocols': '/understand/protocols/',
    '/api/cli': '/reference/cli/',
    '/api/http': '/reference/http-api/',
  },
  // Astro 7 requires Markdown extensions to be configured on a processor.
  // Keep GFM explicit so reference tables render as tables.
  markdown: { processor: unified({ gfm: true }) },
  integrations: [
    starlight({
      title: 'Axocoatl',
      description: 'Install Axocoatl, open the dashboard, create a repository Session, and review your Agent’s work. Guides for Conversation, Files, Preview, Git and headless runs.',
      disable404Route: true,
      favicon: '/favicon.png',
      customCss: ['./src/styles/tokens.css', './src/styles/overrides.css'],
      head: [
        {
          tag: 'link',
          attrs: {
            rel: 'preconnect',
            href: 'https://fonts.googleapis.com',
          },
        },
        {
          tag: 'link',
          attrs: {
            rel: 'preconnect',
            href: 'https://fonts.gstatic.com',
            crossorigin: '',
          },
        },
        {
          tag: 'link',
          attrs: {
            rel: 'stylesheet',
            href: 'https://fonts.googleapis.com/css2?family=Space+Grotesk:wght@400;500;600;700&family=JetBrains+Mono:wght@400;500&display=swap',
          },
        },
      ],
      social: [
        {
          icon: 'github',
          label: 'GitHub',
          href: 'https://github.com/axocoatl/axocoatl',
        },
      ],
      sidebar: [
        {
          label: 'Start',
          items: [
            { label: 'Overview', slug: 'index' },
            { label: 'Install', slug: 'start/install' },
            { label: 'Onboard and doctor', slug: 'start/onboard' },
            { label: 'Your first Session', slug: 'start/first-session' },
          ],
        },
        {
          label: 'Use the workbench',
          items: [
            { label: 'Work in a Session', slug: 'workbench/session' },
            { label: 'Workspace knowledge', slug: 'workbench/knowledge' },
            { label: 'Files, Preview, and Terminal', slug: 'workbench/tools' },
            { label: 'Explore several Ways', slug: 'workbench/ways' },
            { label: 'Review Git and Last turn', slug: 'workbench/git' },
            { label: 'Signed receipts', slug: 'workbench/receipts' },
            { label: 'Context, History, and Stop', slug: 'workbench/history' },
          ],
        },
        {
          label: 'Run headless',
          items: [
            { label: 'Run a fix from the terminal', slug: 'start/first-fix' },
            { label: 'Fix, QA and audit runs', slug: 'workbench/fix-qa-audit' },
            { label: 'Keep as PR', slug: 'workbench/keep-as-pr' },
            // Pages that live under Configure, linked here too; their slugs stay put.
            { label: 'Loadouts', link: '/configure/loadouts/' },
            { label: 'Repository toolchains', link: '/configure/toolchains/' },
            { label: 'External agents', link: '/configure/external-agents/' },
          ],
        },
        {
          label: 'Configure',
          items: [
            { label: 'Settings', slug: 'configure/settings' },
            { label: 'Providers', slug: 'configure/providers' },
            { label: 'Agents and budgets', slug: 'configure/agents' },
            { label: 'Cross-model review', slug: 'configure/cross-model-review' },
            { label: 'Loadouts', slug: 'configure/loadouts' },
            { label: 'Repository toolchains', slug: 'configure/toolchains' },
            { label: 'External agents', slug: 'configure/external-agents' },
            { label: 'The e2e check', slug: 'configure/e2e-check' },
            { label: 'Sandboxes', slug: 'configure/sandboxes' },
            { label: 'Browser', slug: 'configure/browser' },
            { label: 'Web research', slug: 'configure/web' },
            { label: 'Credentials and routes', slug: 'configure/credentials' },
            { label: 'Skills and MCP', slug: 'configure/skills-mcp' },
            { label: 'Automations', slug: 'configure/automations' },
          ],
        },
        {
          label: 'Operate',
          items: [
            { label: 'Run as a service', slug: 'operate/service' },
            { label: 'Data and backup', slug: 'operate/data-backup' },
            { label: 'Upgrade', slug: 'operate/upgrade' },
            { label: 'Uninstall', slug: 'operate/uninstall' },
            { label: 'Security', slug: 'operate/security' },
            { label: 'Resource sizing', slug: 'operate/resources' },
            { label: 'Troubleshooting', slug: 'operate/troubleshooting' },
            { label: 'Verification and release evidence', slug: 'operate/verification' },
          ],
        },
        {
          label: 'Understand',
          items: [
            { label: 'Product model', slug: 'understand/product-model' },
            { label: 'What we measured', slug: 'understand/what-we-measured' },
            { label: 'Architecture', slug: 'understand/architecture' },
            { label: 'State and memory', slug: 'understand/state' },
            { label: 'Coordination and events', slug: 'understand/coordination' },
            { label: 'Protocols', slug: 'understand/protocols' },
          ],
        },
        {
          label: 'Reference',
          items: [
            { label: 'CLI', slug: 'reference/cli' },
            { label: 'axocoatl run', slug: 'reference/run' },
            { label: 'Configuration', slug: 'reference/config' },
            { label: 'HTTP API', slug: 'reference/http-api' },
            { label: 'WebSocket', slug: 'reference/websocket' },
            { label: 'Examples', slug: 'reference/examples' },
          ],
        },
      ],
    }),
    // Keep Cloudflare from rewriting `name@1.2.3` text in code into "[email protected]".
    emailOff(),
  ],
});
