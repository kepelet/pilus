# 0.10 alpha readiness

Written 2026-10-20 on branch `alpha-0.10-readiness`, in both `pilus` and `../sipil`.
Nothing is merged or pushed. Your uncommitted `site/` edits on `main` are untouched
and were never committed.

## Status

- **pilus is close.** Daily use was already solid. I fixed five safety bugs (one of
  them a command-injection), two data-loss risks and the missing log file, and made
  the workspace clippy-clean. All 900+ tests pass.
- **sipil can now coordinate.** It starts a coding agent, watches it, and gets a
  report when the agent finishes or stops on a question; it can send work to
  running agents and carry one agent's words to another. Agents are files you edit,
  reloaded while it runs, and you can run several primary agents from one install,
  which can ask each other. 93 tests pass, plus pilus's 30 integration tests
  against the real daemon.
- **"No SSH tricks" is not reachable from pilus alone.** pilus already uses UHP for
  everything UHP offers. Every remaining shell-over-SSH is for something UHP has no
  method for (file contents, staging, ports, folder listing). That is a list of
  methods to add to Luvus; it is below.

## What changed in pilus

| Commit | What and why |
| --- | --- |
| `6c0d928` | **Command injection under fish.** Remote commands are single-quoted for the host's login shell. fish treats `\'` inside single quotes as an escape, so a path containing `\'` (a file name in a cloned repo is enough) ended the quote and ran the rest. Verified with a payload that printed `PWNED` under fish. Fixed, with a test that runs tricky strings through both `sh` and fish. |
| `4ee7e41` | **Daemon-requested URLs.** `OpenUrl` comes from whatever runs in a pane, remote hosts included, and went straight to the system opener, which launches any scheme (`file:` starts an app). Now only http, https and mailto. |
| `3fc8429` | **SVG logos read local files.** usvg loads `<image href="/any/path">` from disk, so a daemon's logo could read `/dev/zero` until memory ran out, or draw a private image. Only embedded `data:` images now. |
| `9175baa` | **Settings saved atomically**, owner-only from the first byte. A crash mid-save used to leave a broken `settings.json`, which reads as "no settings". `index.db` was world-readable (session titles, machine names); it and `~/.pilus` are owner-only now. |
| `5b8ac0b` | **Remote saves can't truncate files.** A Files-tab save over SSH piped into `cat > file`; a dropped connection left the file cut short. Now staged beside it and copied over only when every byte arrived. Mode and links are kept. |
| `34527fe` | **A log file.** No tracing subscriber was installed, so every warning was dropped and a crash in the bundle left nothing. Now `~/.pilus/logs/pilus.log` (owner-only, rotated at 4 MB). Panics are recorded with thread, location and backtrace. `PILUS_LOG=debug` for more. |
| `f34ae8a` | **Luvus updates fail closed.** Updates verified the checksum only "best effort", so an empty sidecar meant installing unverified. Now required, like the first install. |
| `7b1e477` | **Local Network prompt explained.** iroh dials LAN addresses; the bundle had no `NSLocalNetworkUsageDescription`, so the macOS prompt said nothing and people deny it. |
| `619880a` | **Perf: remote home lookup.** See before/after below. |
| `d324009` | `just clippy` failed on `main`; now clean (an empty `if` left in tick handling, an unused import, test style). Includes rustfmt over files that had drifted. |
| `171d60b` | `AGENTS.md`: a section on untrusted input and local files, so these rules stay. |

### Performance, before and after

Only one change was about speed. The sidebar needed a remote machine's `$HOME` and
got it from a script that ran `git rev-parse` in every workspace, over a fresh SSH
connection, then discarded the git answers. Now it is one `printf` over the shared
SSH connection.

| Host-side cost per lookup (8 workspaces, measured with `/bin/sh` here) | Time |
| --- | --- |
| Before | 66.2 ms, plus about 7 ms per additional workspace |
| After | 4.7 ms, flat |

The SSH handshake saved by reusing the shared connection comes on top of that. I
could not measure it: SSH to localhost needs a host-key prompt, and I left
`known_hosts` alone.

## What changed in sipil

| Commit | What |
| --- | --- |
| `d44f865` | **Report back.** `pilus_start_agent` now watches the agent (a durable task asking Luvus `agent.get` every 5 s). When it goes idle after working, stops on a question (`blocked`) or exits, a report with the end of its screen lands in the chief's conversation and starts its next turn. New tools: `pilus_watch_agent`, `pilus_agent_read`, `pilus_agent_send`. The chief is told to check the work (screen, `git diff`), follow up, and never grant a permission you did not give. |
| `bb2ebf0` | **Blueprints.** `~/.sipil/agents/<id>.md`: frontmatter (`name`, `role`, `summary`, `tools`, `model`, `cwd`, `logo`) plus Markdown instructions. `sipil.md` / `researcher.md` reshape the built-ins. Any `role: subagent` file joins the roster the primaries are shown and the `subagent` tool accepts. **Hot reload:** the folder is watched; the primary is reconfigured at once, a session on its next message. A bad file is reported (log and dashboard) and skipped; the agent keeps what it had. **Several primary agents:** each `role: primary` file runs as its own daemon inside the one process (own device ID, data in `~/.sipil/primaries/<id>/`, shared allow-list), so pilus needs no change to show them. |
| `45d12d2` | **`sipil tell`.** A coding agent can write to the chief from its terminal: `node <sipil>/src/main.ts tell "Done: …"`. The daemon names the sender from `LUVUS_PANE_ID`. The chief is told the exact command, so it can ask an agent for its own report. |
| `d2277c2` | Reports, finished subagents and told messages used to show in pilus under **your** nick. They now carry `speaker`, which pilus already draws as the nick. Optional field: no protocol bump. |
| `b3cdf7a` | A **new primary asks your trusted pilus devices to add it**, once; you confirm in pilus. |
| `27d71a6` | **Primary agents ask each other** (`ask_agent`). Asynchronous both ways, so two agents can't deadlock; 6 questions per pair per 10 minutes. |
| `4d527b0` | `agent.start` got the same 15 s deadline as every call; a cold agent can take longer, and a timeout read as "not started", inviting a duplicate. Now 2 minutes, and it checks the agent list before calling it a failure. |
| `1d0fb47` | Dashboard shows unusable blueprints, each agent's blueprint and model, and the other primaries. |

Try it: copy `../sipil/examples/agents/*.md` into `~/.sipil/agents/`, start sipil,
and edit a file while it runs. Every blueprint is a primary agent (there are no
subagents any more: `role: subagent`, the `subagent` tool and the researcher are
gone, and pilus has no subthreads panel). `ops.md` and `reviewer.md` each ask your
pilus to add them.

## SSH and UHP

pilus talks to Luvus over UHP for everything UHP has: agents, tabs, panes, tasks,
automations, mission, workspaces, worktrees, the file tree (`files.tree`), git
state (`git.status`, `git.log`), the diff and its notes. For a remote machine that
UHP travels as `luvus uhp proxy` over SSH, which is Luvus's own remote design, not
a trick.

What still runs as a shell script over SSH, and what Luvus would need so it doesn't:

| pilus feature (file) | What the script does | UHP today | Luvus method that would replace it |
| --- | --- | --- | --- |
| Files tab open/save (`read.rs`, `write.rs`) | `cat`, `git show :path` | none (`files.*` never reads or writes) | `files.read {path, version}`, `files.write {path, text, if_revision}` |
| New/rename/delete file (`entries.rs`) | `mkdir`, `mv`, `rm` | none | `files.create`, `files.rename`, `files.delete` |
| Folder picker (`folders.rs`) | `ls` a directory | `files.tree` is per workspace only | `files.list {path}` |
| Upload/download (`upload.rs`, `download.rs`) | `cat` / `tar` over the pipe | none | a streaming `files.put` / `files.get` (or keep SSH; it is a transfer) |
| Stage/unstage/discard (`stage.rs`, `git.rs`) | `git add/restore/reset/clean` | `git.*` is read-only | `git.stage`, `git.unstage`, `git.discard` |
| Commit detail (`commit.rs`) | `git show` | `git.log` has no diff | `git.show {sha}` |
| Worktree dirty check (`git.rs`) | `git status --porcelain` | `worktree.list` has no dirty flag | `dirty` on `worktree.list` |
| Installed coding agents (`agents.rs`) | probe dirs/binaries | `server.agent_manifests` lists launch rules, not what is installed | `server.agents_installed` (with model catalogs) |
| Ports tab (`ports.rs`) | `lsof`/`ss`, `kill`, `ssh -L` | `pane.processes` has no ports | `ports.list`, `ports.kill`; forwarding stays SSH (it is a transport) |
| Session search/stats (`pilus-index`) | `grep`/`tail`/`awk` on transcripts | `agent.sessions` lists resumable sessions only | `agent.sessions` with title, time and token use |
| Remote `$HOME` (sidebar) | `printf "$HOME"` | none clean | `home` in `uhp.capabilities` or `session.snapshot` |

My recommendation: ask Luvus for the `files.*` and `git.stage/unstage/discard`
methods first. They cover the Files and Git tabs, which are most of the remaining
shell traffic and most of the quoting surface. Ports and transfers can stay on SSH.

sipil uses UHP only, never SSH, and now only through pilus-agent's hub (below).

## Safety: what is left

- **Luvus updates are checksum-only.** The `.sha256` comes from the same release
  host as the archive, so it proves integrity, not authenticity. pilus's own
  updates are minisign-signed; ask Luvus to sign releases the same way and verify
  here.
- **Notarize the alpha.** `package-macos-dmg.sh` notarizes only when
  `NOTARY_PROFILE` is set. A public alpha without Developer ID + notarization is
  blocked by Gatekeeper and teaches people to bypass it.
- **`file:` links from the daemon no longer open.** If Luvus deliberately sends
  `file://` (for example `ls --hyperlink`), clicking does nothing now. If you want
  them, allow `file:` only for a local session and never for `.app` bundles.
- **pilus-agent hub, operate level.** Documented and opt-in, but on a saved SSH
  machine operate means full owner authority there (`luvus uhp proxy`). Worth one
  sentence in the public docs.
- **`~/.sipil/credentials.json`** is left over from the old key API (holds a
  provider key, mode 0600). sipil no longer reads it; delete it when convenient. I
  did not touch it.
- sipil's tools run as you with your keys, and a paired device can drive them.
  That is the design (the README says to treat the allow-list like SSH keys). The
  new `sipil tell` and `ask_agent` messages are marked as agent words, not yours,
  but that is prompt guidance, not a boundary.

## sipil as chief of staff: what is left

1. **Other machines: done.** sipil now reaches pilus only through pilus-agent's
   hub, as an enrolled device (`sipil hub pair '<link from Add device>'`, at the
   operate level), so it can run on another machine than pilus. Every pilus tool
   takes a `machine` from `pilus_machines`. pilus-agent gained `args` on
   `hub.run`, `provider`/`id` per model in `hub.agents`, and a two-minute limit on
   `hub.run`. What does not follow sipil off the pilus machine: `sipil tell` (a
   local file drop) and the chief checking `git diff` itself.
2. **Approval gate.** The CoS prototype asked before every external write; sipil
   acts directly (you said sipil should coordinate, so I kept that). If you want a
   gate, the place is `pilus_start_agent` / `pilus_agent_send`: queue, then a
   pilus button to approve.
3. **Worktree isolation.** The chief starts agents in the workspace itself. For
   parallel work on one repo it should create a worktree first (`worktree.create`).
   I did not add this: testing it would have meant creating worktrees in your repos
   overnight.
4. **Editing blueprints from pilus.** The About panel shows instructions. An
   "edit" there needs a call that writes the blueprint file, which is a new
   permission. I left it for you to decide.
5. Reports are read from the agent's **screen** (`agent.read`), so they include
   whatever TUI chrome survives the filter. `sipil tell` gives a clean report when
   the agent uses it.

## Judgment calls I made for you

- **Several primary agents = several daemons in one process**, not several agents
  on one daemon. pilus assumes one primary per daemon in the sidebar, mentions,
  offline cache and channel invites; this way none of that changes. The cost is
  one device ID and one pairing per primary, which the automatic introduction
  softens.
- **sipil's default primary keeps its old place** (`~/.sipil`). Extra primaries go
  in `~/.sipil/primaries/<id>/`. The stdout lines `device id:` and `ticket:` are
  unchanged, because pilus's integration test reads them.
- **No version bump.** The workspace says `0.3.0`. Bump to `0.10.0` (or
  `0.10.0-alpha.1`) yourself when you cut it, since the OTA channels compare
  versions.
- I fixed the existing clippy and rustfmt drift because `just clippy` failed on
  `main`. It is one isolated commit (`d324009`) if you would rather drop it.

## Before you tag

- [ ] Review and merge `alpha-0.10-readiness` in both repos.
- [ ] Bump versions (pilus workspace, sipil `package.json`).
- [ ] Package with `APPLE_DEVELOPER_IDENTITY`, `NOTARY_PROFILE` and
      `MINISIGN_SECRET_KEY` set; check `publish-release.sh` refuses an unsigned DMG.
- [ ] Run the app once from the DMG (not `cargo run`) and check
      `~/.pilus/logs/pilus.log` appears and stays quiet.
- [ ] In sipil: put a `reviewer.md` in `~/.sipil/agents`, ask the chief to start
      an agent on a small task, and watch the report arrive in pilus under the
      agent's nick.
