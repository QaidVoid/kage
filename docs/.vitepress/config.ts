import { defineConfig } from "vitepress";

export default defineConfig({
  title: "kage",
  description: "A coding agent for your terminal, your editor and your browser.",
  appearance: "dark",
  cleanUrls: true,
  lastUpdated: true,
  head: [
    ["link", { rel: "icon", type: "image/svg+xml", href: "/favicon.svg" }],
    ["meta", { name: "theme-color", content: "#0a0a0a" }],
    ["meta", { name: "viewport", content: "width=device-width, initial-scale=1" }],
  ],
  markdown: {
    theme: {
      light: "github-light",
      dark: "vitesse-dark",
    },
    lineNumbers: false,
  },
  themeConfig: {
    nav: [
      { text: "guide", link: "/guide/install" },
      { text: "plugins", link: "/plugins/" },
      { text: "editors", link: "/editors/zed" },
      { text: "reference", link: "/reference/architecture" },
    ],
    sections: [
      {
        label: "guide",
        glyph: "01",
        items: [
          { text: "install", link: "/guide/install" },
          { text: "quickstart", link: "/guide/quickstart" },
          { text: "keybindings", link: "/guide/keybindings" },
          { text: "commands", link: "/guide/commands" },
          { text: "configuration", link: "/guide/config" },
          { text: "lua config", link: "/guide/lua-config" },
          { text: "providers", link: "/guide/providers" },
          { text: "themes", link: "/guide/themes" },
          { text: "desktop and web", link: "/guide/desktop" },
          { text: "mcp", link: "/guide/mcp" },
          { text: "permissions", link: "/guide/permissions" },
          { text: "agents", link: "/guide/agents" },
          { text: "plan mode", link: "/guide/plan-mode" },
          { text: "questions", link: "/guide/questions" },
        ],
      },
      {
        label: "plugins",
        glyph: "02",
        items: [
          { text: "overview", link: "/plugins/" },
          { text: "lua api", link: "/plugins/api" },
          { text: "capabilities", link: "/plugins/capabilities" },
          { text: "editor setup", link: "/plugins/editor" },
          { text: "examples", link: "/plugins/examples" },
        ],
      },
      {
        label: "editors",
        glyph: "03",
        items: [
          { text: "zed", link: "/editors/zed" },
          { text: "neovim", link: "/editors/neovim" },
          { text: "acp client", link: "/editors/acp-client" },
          { text: "remote", link: "/editors/remote" },
          { text: "kage extensions", link: "/editors/kage-extensions" },
        ],
      },
      {
        label: "reference",
        glyph: "04",
        items: [
          { text: "architecture", link: "/reference/architecture" },
        ],
      },
    ],
    socialLinks: [
      { icon: "github", link: "https://github.com/QaidVoid/kage" },
    ],
  },
});
