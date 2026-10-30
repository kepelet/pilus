import * as Alchemy from "alchemy";
import * as Cloudflare from "alchemy/Cloudflare";
import * as Config from "effect/Config";
import * as Effect from "effect/Effect";
import { workerFirstGlobs } from "./site/worker.ts";

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
    // to `site/dist`. Only the landing page and the files it loads are public:
    // `runWorkerFirst` sends every other path to the Worker, which answers 404
    // (see `site/worker.ts`), so the built docs and blog stay unpublished.
    const site = yield* Cloudflare.Worker("Site", {
      main: "./site/worker.ts",
      assets: {
        directory: "../site/dist",
        htmlHandling: "auto-trailing-slash",
        notFoundHandling: "none",
        runWorkerFirst: workerFirstGlobs(),
      },
      domain: "usepilus.dev",
    });

    // The APNs relay (see `push/worker.ts`). The host daemon POSTs here when an
    // agent starts waiting; this Worker alone holds the Apple key. Secrets come
    // from the deploy environment, never from the repo.
    const push = yield* Cloudflare.Worker("Push", {
      main: "./push/worker.ts",
      domain: "push.usepilus.dev",
      env: {
        APNS_KEY: Config.Redacted("PILUS_APNS_KEY"),
        APNS_KEY_ID: Config.Redacted("PILUS_APNS_KEY_ID"),
        APNS_TEAM_ID: Config.Redacted("PILUS_APNS_TEAM_ID"),
        APNS_TOPIC: "com.kepelet.pilus",
      },
    });

    return {
      siteUrl: site.url,
      pushUrl: push.url,
      releasesBucket: releases.bucketName,
    };
  }),
);
