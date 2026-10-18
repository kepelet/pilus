import * as Alchemy from "alchemy";
import * as Cloudflare from "alchemy/Cloudflare";
import * as Config from "effect/Config";
import * as Effect from "effect/Effect";

export default Alchemy.Stack(
  "Pilus",
  {
    providers: Cloudflare.providers(),
    state: Cloudflare.state(),
  },
  Effect.gen(function* () {
    // The bucket predates this stack and holds the DMGs and channel manifests
    // live clients read, so it is declared under its real name. Alchemy then
    // observes it instead of creating a new one, and its objects are untouched.
    const bucketName = yield* Config.String("PILUS_RELEASES_BUCKET");

    const releases = yield* Cloudflare.R2.Bucket("Releases", {
      name: bucketName,
      domains: [{ name: "releases.kepelet.com" }],
    });

    // `site/` is a Blume project; `bun run build` there writes the static site
    // to `site/dist`, which ships as an assets-only Worker.
    const site = yield* Cloudflare.Worker("Site", {
      assets: {
        directory: "../site/dist",
        htmlHandling: "auto-trailing-slash",
        notFoundHandling: "404-page",
      },
      domain: "usepilus.dev",
    });

    return {
      siteUrl: site.url,
      releasesBucket: releases.bucketName,
    };
  }),
);
