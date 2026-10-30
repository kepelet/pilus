/**
 * usepilus.dev's only server-side behaviour: the landing page is the whole
 * published site, and every other path answers 404. The docs, blog and
 * changelog are built but not announced, and a page that is not ready should
 * look like it does not exist rather than like a half-finished page.
 *
 * The asset layer serves everything in `PUBLIC`: the landing page and the files
 * it loads. `alchemy.run.ts` routes every other path to this Worker first (see
 * `workerFirstGlobs`), so a built page, its markdown twin, `sitemap.xml` and
 * the rest never reach the asset layer. Publishing a section again means adding
 * its prefix to `PUBLIC`; update the landing page's links at the same time.
 *
 * This cannot live in `_redirects`: a 404 is not a redirect, and the asset
 * router matches redirect rules ahead of the manifest.
 */

/**
 * Paths the asset layer still serves: the landing page (`/`, which the
 * `auto-trailing-slash` handling resolves to `index.html`) and the stylesheet,
 * scripts, fonts and images it loads. `/*` entries cover a whole directory.
 */
export const PUBLIC = ["/", "/index.html", "/tokens.css", "/_astro/*", "/assets/*", "/fonts/*"];

/**
 * `runWorkerFirst` rules for `alchemy.run.ts`: everything runs the Worker
 * first except {@link PUBLIC}. A negated rule wins over `/*`.
 */
export function workerFirstGlobs(allowed: readonly string[] = PUBLIC): string[] {
  return ["/*", ...allowed.map((path) => `!${path}`)];
}

/**
 * A bare 404, not Blume's `404.html`: that page links to the blog, the
 * changelog and the sitemap, which would advertise the pages this Worker is
 * hiding. Nothing here mentions them.
 */
const NOT_FOUND_HTML = `<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><meta name="robots" content="noindex"><title>404 — pilus</title>
<style>body{margin:0;min-height:100vh;display:grid;place-items:center;background:#0e0e10;color:#f5f2ec;font:400 16px/1.6 Arimo,system-ui,sans-serif}main{text-align:center;padding:2rem}a{color:#e99a24}</style></head>
<body><main><h1>404</h1><p>This page does not exist.</p><p><a href="/">Back to home</a></p></main></body></html>
`;

function notFound(): Response {
  return new Response(NOT_FOUND_HTML, {
    status: 404,
    headers: { "content-type": "text/html; charset=utf-8" },
  });
}

export default {
  fetch: (): Response => notFound(),
};
