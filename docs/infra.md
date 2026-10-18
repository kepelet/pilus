# Infrastructure

Cloudflare is managed with [Alchemy](https://alchemy.run) from `infra/alchemy.run.ts`.
The stack declares two things:

| Resource | What it serves |
| --- | --- |
| `Site` (Worker, assets only) | `site/dist`, the built landing page and docs, at `usepilus.dev` |
| `Releases` (R2 bucket) | the OTA payloads at `releases.kepelet.com` |

Both zones already exist in the account, so they are not declared: Alchemy
infers each zone from the hostname and attaches the custom domain and its DNS
record and certificate. Alchemy manages the bucket, not its contents. DMGs and
the `stable.json` / `preview.json` manifests are still uploaded as described in
[ota.md](ota.md).

## First deploy

```sh
cd infra
bun install
bunx alchemy profile edit --add Cloudflare   # OAuth or API token, once per machine
echo 'PILUS_RELEASES_BUCKET=<existing bucket name>' > .env
bunx alchemy plan --stage prod               # read this before deploying
bunx alchemy deploy --stage prod
```

`PILUS_RELEASES_BUCKET` must be the name of the bucket that already backs
`releases.kepelet.com`. Alchemy observes a bucket with that name instead of
creating one, so its objects are untouched. A wrong name would create an empty
bucket.

A hostname can belong to one thing at a time. Before the first deploy, detach
`usepilus.dev` from the old Pages project (or Worker), or the deploy fails with
a custom-domain conflict. `releases.kepelet.com` is already on the bucket, so
it should show as unchanged in the plan; if it shows as created, stop and check
the bucket name.

`site/` is a [Blume](https://useblume.dev) project: `pages/index.astro` is the landing
page at `/`, `docs/*.md` are the docs under `/docs`, and `public/` holds the static assets.
`just site-build` writes `site/dist`, which is what the Worker uploads; `infra-plan` and
`infra-deploy` run it first. `just site-dev` serves the site locally.

State lives in a state-store Worker in the Cloudflare account (`Cloudflare.state()`),
which Alchemy bootstraps on the first deploy. `just infra-plan` and
`just infra-deploy` wrap the commands above.
