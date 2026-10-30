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
  `agent`/`pane` events (400 ms settle), and `BlockedWatch` picks out agents that
  just became `blocked`. An agent already blocked at the first look is old news.
  Only the paired machine is covered: an SSH machine has no event feed, so the
  phone's local alert still serves it.
- **Relay** (`infra/push`). Validates the body, signs an ES256 provider token
  (cached 40 minutes), and calls APNs. The phone's `sandbox` flag is a hint:
  on `BadDeviceToken` it tries the other environment, so a Debug/TestFlight mix-up
  does not silently drop alerts. APNs 410 or `BadDeviceToken` in both
  environments becomes 410, and the host forgets the token.

## The alert

Title `<Agent> is waiting your input`, body the session's title (else the
pane's alias, else the workspace). The collapse id is
`pilus-blocked-<machine>-<pane>`, the same tag the app uses locally
(`AttentionWatch.swift`), so a newer alert replaces an older one and the app
can take a delivered one down once the agent is unblocked. Keep the two tags in
step; APNs caps the id at 64 bytes.

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
- `devices.json` stays the only authority. A registration is dropped when its
  device is revoked (checked before every send).
- Never log a token, a title or a body. The relay logs none either.
- `push.json` is disposable: phones register again on connect, so a file that
  does not decode starts empty rather than blocking the daemon.
