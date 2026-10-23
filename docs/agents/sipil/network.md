# Sipil: the network

Read this before you change the sipil wire, the node, identity, or who is
allowed to call what.

## One protocol over iroh

Every sipil is a node on one protocol over iroh. That includes the durable
agent daemon (the `../sipil` project, a Node service over pi-durable) and every
pilus window.

- ALPN is `pilus/sipil/1`. Each QUIC stream carries one JSON call:
  `{"method","params"}` in, and `{"result"}` or `{"error":{"code","message"}}`
  out (see `pilus_client::sipil::wire`).
- It is not the Luvus protocol, so `pilus-protocol` is not involved. There is
  no HTTP either: sipil binds no port.
- A node is addressed by its **device ID** (the iroh `EndpointId`, 64 hex
  characters) or by a ticket. The caller's identity is whatever the handshake
  proved, never a field in the frame.
- `wire::API_VERSION` is the contract version (version 1 was the old HTTP API).
  The daemon's `src/rpc.ts` and `wire.rs` are two ends of one contract, so
  change them together and extend `crates/pilus-client/tests/sipil.rs`. Its
  last test drives the real daemon when `../sipil` has its `node_modules`.

## Who may call what

- `health` answers anyone. It says what kind of node it is (`daemon` or
  `pilus`) and whether the asker is allowed.
- Every other method needs an allow-list entry. On a daemon, the allow-list is
  filled by `npm run connect` once a pilus accepts, or by `npm run allow`. On a
  pilus, the allow-list is its contacts.
- An allowed device on a daemon can run that daemon's agents' shell tools, so
  treat the allow-list like SSH keys.

## The node and identity

- `pilus_client::sipil::Node` owns one iroh endpoint and a small tokio runtime
  behind blocking calls. The GUI thread never touches it.
- `Node::ticket` also lists loopback, so a node on the same machine is reached
  directly.
- The identity key is `~/.pilus/sipil/identity.key`. Losing it changes the
  device ID everywhere it was added.
- The key is created the first time the Sipil tab opens (`SipilPanel::new`
  calls `sipil::prepare`), not at launch, so a person who never opens sipil
  never gets a key file. `visual-test` captures never write it.

## A pilus is a sipil (`peer::Local`)

`peer::Local` bundles the node, the contacts (the `contacts` document), the
conversations (the `chat_*` tables) and the `PeerService` that answers inbound
calls. Storage details are in [persistence.md](../persistence.md). The identity
key stays under `~/.pilus/sipil`, and the old `contacts.json` and `chats.jsonl`
there are imported once.

`PeerService` answers four methods: `health`, `pair.request`, `chat.send` and
`chan.send`. It answers `agent.*` with `unknown_method`.

- `Settings::sipil_online` (off by default) makes this pilus reachable and
  starts the node at launch. When it is off, the node still starts for a user
  action (connect, add a contact, send) but answers nobody.

## Pairing a daemon

A daemon is never typed in here.

1. `sipil connect` on the daemon's machine sends `pair.request`.
2. `PeerService` records it as a `Request` with `daemon: true`.
3. Accepting it (`Directory::accept`) makes a `Contact` with `daemon: true`.
4. `sipil::ensure` dials the daemon contacts at launch.

A daemon contact is never a person: `Directory::contacts`, `contact` and
`is_contact` skip it, so nothing that admits a person admits a daemon.

Provider keys and the model belong to the daemon, which reads them from its own
environment (`OPENCODE_API_KEY`, `SIPIL_MODEL`). The protocol has no call to
read, set or choose either, so pilus has no key form and no model picker. The
chat page only shows the model a conversation runs on.
