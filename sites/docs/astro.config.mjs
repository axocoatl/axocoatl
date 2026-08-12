// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import remarkGfm from 'remark-gfm';

// https://astro.build/config
export default defineConfig({
  site: 'https://docs.axocoatl.ai',
  // Starlight's current Astro 6 integration replaces Astro's default remark
  // pipeline. Restore GFM explicitly so reference tables render as tables
  // instead of shipping their pipe syntax as paragraphs.
  markdown: { remarkPlugins: [remarkGfm] },
  integrations: [
    starlight({
      title: 'Axocoatl',
      description: 'A local-first coding workbench backed by a durable Rust agent runtime.',
      favicon: '/favicon.svg',
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
          label: 'Start here',
          items: [
            { label: 'Introduction', slug: 'index' },
            { label: 'Getting started', slug: 'getting-started' },
          ],
        },
        {
          label: 'Concepts',
          items: [
            { label: 'Sessions and attempts', slug: 'concepts/sessions' },
            { label: 'Agents', slug: 'concepts/agents' },
            { label: 'Coordinator', slug: 'concepts/coordinator' },
            { label: 'Skills', slug: 'concepts/skills' },
            { label: 'Memory', slug: 'concepts/memory' },
            { label: 'Checkpointing', slug: 'concepts/checkpointing' },
            { label: 'Automations', slug: 'concepts/automations' },
            { label: 'The event lattice', slug: 'concepts/lattice' },
            { label: 'Protocols (MCP & A2A)', slug: 'concepts/protocols' },
          ],
        },
        {
          label: 'Guides',
          items: [
            { label: 'Ollama quickstart', slug: 'guides/ollama-quickstart' },
            { label: 'Architecture', slug: 'guides/architecture' },
            { label: 'Providers', slug: 'guides/providers' },
            { label: 'Token budgets', slug: 'guides/token-budgets' },
            { label: 'Tool approval', slug: 'guides/tool-approval' },
            { label: 'Troubleshooting', slug: 'guides/troubleshooting' },
            { label: 'Examples gallery', slug: 'guides/examples-gallery' },
          ],
        },
        {
          label: 'Reference',
          items: [
            { label: 'HTTP API', slug: 'api/http' },
            { label: 'CLI', slug: 'api/cli' },
          ],
        },
      ],
    }),
  ],
});
