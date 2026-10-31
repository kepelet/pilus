/**
 * usepilus.dev's only server-side behaviour: short links that leave the site,
 * and `/download`, which follows the latest stable DMG.
 * Everything else, the landing page, docs, blog and changelog included, is
 * served straight from the asset layer, with Blume's own `404.html` for
 * unknown paths.
 *
 * `alchemy.run.ts` runs this Worker first only for the paths in
 * {@link REDIRECTS} and {@link DOWNLOAD_PATH} (see `workerFirstGlobs`), so no other request reaches it.
 *
 * This cannot live in `_redirects`: the asset router would need a rule that
 * points off-site, and keeping every redirect in one typed map next to its
 * test is easier to review than a second file format.
 */

/** Short links and where they go. */
export const REDIRECTS: Readonly<Record<string, string>> = {
  "/ios": "https://testflight.apple.com/join/cdZPNhVS",
};

/**
 * `/download` follows the stable channel's manifest, the same file the app's
 * updater reads, so publishing a release (`publish-release.sh stable`) moves
 * the download button with no site deploy and no second copy of the DMG.
 */
export const DOWNLOAD_PATH = "/download";
export const STABLE_MANIFEST = "https://releases.kepelet.com/pilus/stable.json";
const RELEASES_ORIGIN = "https://releases.kepelet.com";

/**
 * `runWorkerFirst` rules for `alchemy.run.ts`: each redirect path, with and
 * without the trailing slash.
 */
export function workerFirstGlobs(paths: readonly string[] = [...Object.keys(REDIRECTS), DOWNLOAD_PATH]): string[] {
  return paths.flatMap((path) => [path, `${path}/`]);
}

type Fetcher = (input: string) => Promise<Response>;

/** The macOS DMG URL the stable manifest advertises, or `undefined` when it cannot be trusted. */
export async function latestDmgUrl(fetcher: Fetcher = fetch): Promise<string | undefined> {
  try {
    const response = await fetcher(STABLE_MANIFEST);
    if (!response.ok) return undefined;
    const manifest = (await response.json()) as { assets?: { os?: string; format?: string; url?: string }[] };
    const url = manifest.assets?.find((asset) => asset.os === "macos" && asset.format === "dmg")?.url;
    // The manifest is data, not a place to send visitors: only our own bucket.
    return url?.startsWith(`${RELEASES_ORIGIN}/`) ? url : undefined;
  } catch {
    return undefined;
  }
}

export async function handle(request: Request, fetcher: Fetcher = fetch): Promise<Response> {
  const path = new URL(request.url).pathname.replace(/\/+$/, "");

  if (path === DOWNLOAD_PATH) {
    const url = await latestDmgUrl(fetcher);
    // 302: the target changes with every release, so it must not be cached.
    return url ? Response.redirect(url, 302) : new Response("The download is unavailable right now.", { status: 502 });
  }

  const target = Object.hasOwn(REDIRECTS, path) ? REDIRECTS[path] : undefined;
  // 302, not 301: the invite link is rotated by TestFlight, and a browser
  // must not cache the old one for good.
  return target ? Response.redirect(target, 302) : new Response(null, { status: 404 });
}

export default {
  fetch: (request: Request): Promise<Response> => handle(request),
};
