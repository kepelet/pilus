import { defineComponents } from "blume";

export default defineComponents({
  layout: {
    Breadcrumbs: "./components/Breadcrumbs.astro",
    Sidebar: "./components/Sidebar.astro",
  },
  mdx: {
    BlogPosts: "./components/BlogPosts.astro",
  },
});
