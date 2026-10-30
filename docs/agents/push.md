# Push alerts on iOS

Read this before you change how a sleeping phone is told that an agent is
waiting: `crates/pilus-agent/src/push.rs`, the `hub.push.*` methods,
`infra/push`, or `ios/Pilus/Notifier.swift`.

## Why a relay

The phone and the host talk peer to peer, and iOS suspends the app soon after
it leaves the foreground, so the app cannot watch for an agent that blocks. The
host can: `pilus-agent` already holds the Luvus event feed. Only Apple can wake
the phone, and that takes an APNs key, which must not sit on users' machines.
The key lives in a Cloudflare Worker (`infra/push/worker.ts`); the host sends
it one POST per alert.

```
agent blocks → Luvus event → pilus-agent (push.rs) → relay Worker → APNs → phone
```

## Pieces

- **Phone** (`Notifier.swift`, `AppStore.syncPush`). After the user allows
  notifications it calls `registerForRemoteNotifications`, `PushDelegate`
  receives the token, and `hub.push.register {token, sandbox}` hands it to the
  paired host. The reply's `registered` says whether the host will push; only
  then does the local alert step aside (`Notifier.hostPushes`). Turning the
  Settings toggle off, or disconnecting, calls `hub.push.unregister`.
- **Host** (`push.rs`). `PushStore` keeps tokens per enrolled device in
  `~/.pilus/push.json`, owner-only and written atomically. `run` holds the
  event feed **only while a token is registered**, reads `agent.list` after
  `agent`/`pane` events (400 ms settle), and `AttentionWatch` picks out agents that
  just became `blocked` and agents that went from `working` to `done`. An agent
  already blocked or done at the first look is old news, and so is one that shows
  up `done` without having been seen `working`.
  A saved SSH machine has no event feed (`luvus uhp proxy` is one request per
  SSH call), so `run_remotes` polls it instead: one `agent.list` per enabled
  machine with a verified remote binary (the hub's own `ready` test), each with
  its own `AttentionWatch`. The gap is 10 s while one of the machine's agents is
  `working` or `blocked` and 30 s otherwise (`next_poll`). The catalog is re-read every minute. An
  unreachable machine backs off to five minutes and is logged once per outage.
  With no token registered, nothing is listed and no SSH call is made.
- **Relay** (`infra/push`). Validates the body, signs an ES256 provider token
  (cached 40 minutes), and calls APNs. The phone's `sandbox` flag is a hint:
  on `BadDeviceToken` it tries the other environment, so a Debug/TestFlight mix-up
  does not silently drop alerts. APNs 410 or `BadDeviceToken` in both
  environments becomes 410, and the host forgets the token.

## The alert

Title `<Agent> is waiting your input` when an agent blocks and `<Agent> finished`
when a running turn ends. The body is the session's title (else the pane's
alias, else the workspace). The collapse id is `pilus-blocked-<machine>-<pane>`
or `pilus-done-<machine>-<pane>`, with `local` or the saved machine's id, the
same tags the app uses locally (`AttentionWatch.swift`), so a newer alert
replaces an older one of its kind, the two kinds never replace each other, and
the app can take a delivered one down once the agent is unblocked or starts a
new turn. Keep the two tags in step; APNs caps the id at 64 bytes, and
`tag_for` cuts on a character boundary because a machine id is user-named.

## Setup

1. Apple: enable Push Notifications on the `com.kepelet.pilus` App ID and make
   an APNs auth key (`.p8`).
2. Deploy with `PILUS_APNS_KEY` (the `.p8` text), `PILUS_APNS_KEY_ID` and
   `PILUS_APNS_TEAM_ID` in the environment and deploy the Alchemy stack in `infra`.
3. The host posts to `https://push.usepilus.dev/v1/push` unless
   `PILUS_PUSH_URL` says otherwise. Set it empty to turn push off; phones then
   keep their local alerts.

## Rules

- `hub.push.*` is answered by `Hub::handle_push`, not `Hub::handle`: it needs the
  caller's endpoint id, and the token is stored under that id whatever the
  params claim. Any enrolled level may call it.
- The host's SSH calls run with `BatchMode=yes`, so a machine whose key needs a
  passphrase or a Touch ID tap fails quietly, as a service has nobody to ask.
  Check with `env -i HOME=$HOME PATH=/usr/bin:/bin ssh -o BatchMode=yes <dest> true`.
- `devices.json` stays the only authority. A registration is dropped when its
  device is revoked (checked before every send).
- Never log a token, a title or a body. The relay logs none either.
- The relay takes no credential, by decision. Sending needs the device token,
  which only the phone, Apple and the user's host hold; a shared secret in the
  daemon would ship in every copy and stop no one. Two Cloudflare rate limits
  in `infra/alchemy.run.ts` bound the rest: 30 requests a minute per caller IP
  (cost, and the APNs key's standing) and 20 a minute per token (spam to one
  phone). A limiter that errors lets the alert through. The host treats a 429
  as a failed send, not a gone phone. If abuse shows up, the next step is a
  per-host credential the relay issues and verifies, not a shared key.
- `push.json` is disposable: phones register again on connect, so a file that
  does not decode starts empty rather than blocking the daemon.
