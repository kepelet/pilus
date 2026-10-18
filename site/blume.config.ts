import { defineConfig } from "blume";

export default defineConfig({
  title: "pilus",
  description:
    "pilus is a native Mac workspace for Luvus: persistent agent sessions, live agent state, and local or SSH machines in one window.",
  logo: { image: "/assets/pilus-mark.svg", text: "pilus" },
  // The landing page is `pages/index.astro` at `/`, so no docs page can be the
  // root: the first docs page is /installation and /docs is a plain index page.
  theme: {
    accent: "#e99a24",
    mode: "system",
    fonts: {
      display: { name: "Arimo", weights: ["400..700"] },
      body: { name: "Arimo", weights: ["400..700"] },
      mono: {
        name: "Ioskeley Mono",
        variants: [
          { src: "./public/fonts/IoskeleyMono-Regular.woff2", weight: 400, style: "normal" },
          { src: "./public/fonts/IoskeleyMono-Bold.woff2", weight: 700, style: "normal" },
          { src: "./public/fonts/IoskeleyMono-Italic.woff2", weight: 400, style: "italic" },
          { src: "./public/fonts/IoskeleyMono-BoldItalic.woff2", weight: 700, style: "italic" },
        ],
      },
    },
  },
  // Blog and Changelog are plain links under FAQ rather than groups, so their
  // posts and releases stay out of the sidebar. An explicit sidebar replaces the
  // generated tree: add new docs pages here or they will not be listed.
  navigation: {
    sidebar: [
      "/docs",
      "/installation",
      "/quickstart",
      "/faq",
      { label: "Guides", href: "/guides", icon: "book-open" },
      { label: "Blog", href: "/blog", icon: "newspaper" },
      { label: "Changelog", href: "/changelog", icon: "history" },
      {
        label: "Fitur utama",
        items: [
          "/features/ui",
          "/features/in-app-browser",
          "/features/port-forwarder",
          "/features/git",
          "/features/github",
          "/features/file-explorer",
          "/features/ios",
        ],
      },
      {
        label: "Panduan",
        items: ["/guides/connect-remote"],
      },
    ],
  },
  deployment: {
    site: "https://usepilus.dev",
  },
});
