import { defineConfig } from "vitepress";

export default defineConfig({
  title: "kage",
  description: "A minimal, extensible coding agent in your terminal.",
  appearance: "dark",
  cleanUrls: true,
  lastUpdated: true,
  head: [
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
          { text: "themes", link: "/guide/themes" },
        ],
      },
      {
        label: "plugins",
        glyph: "02",
        items: [
          { text: "overview", link: "/plugins/" },
          { text: "lua api", link: "/plugins/api" },
          { text: "editor setup", link: "/plugins/editor" },
          { text: "examples", link: "/plugins/examples" },
        ],
      },
      {
        label: "reference",
        glyph: "03",
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
