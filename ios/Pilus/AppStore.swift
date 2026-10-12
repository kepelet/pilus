import Foundation

@MainActor
@Observable
final class AppStore {
    var client: PilusClient?
    /// Workspaces/agents/tasks merged across every reachable machine; each
    /// element carries its `machine` tag.
    var workspaces: [Workspace] = []
    var agents: [Agent] = []
    var tasks: [OrchTask] = []
    var machines: [Machine] = []
    var hostLabel: String?
    /// Agent-kind catalog per machine (`hub.agents` answers per machine).
    var agentKindsByMachine: [String: [HubAgentKind]] = [:]
    var pseudoKindsByMachine: [String: [String]] = [:]
    /// Machines to merge lists over: `local` first, then usable SSH machines
    /// in `hub.machines` order.
    var machineOrder: [String] = ["local"]
    var busy = false
    /// False until the first `refresh` completes — drives the skeleton on
    /// cold start, which `busy` alone misses (enroll/connect precede it).
    var loaded = false
    var error: String?
    var deviceId: String?
    var enrolledHost: String?
    /// The device's level on the hub (`view`/`control`/`operate`); nil on a
    /// pre-hub connector where the token's surface is all we know.
    var level: String?
    /// What the connector says this device may call (the hub rewrites it per
    /// level). Empty until the first probe answers.
    var allowedMethods: Set<String> = []

    /// True while `hub.watch` is open. The detail view polls slowly then and
    /// leans on `outputSignals` instead.
    private(set) var watchLive = false
    /// Bumped for `machine:pane` each time the watch reports output from that
    /// pane — says where something was written, never what.
    private(set) var outputSignals: [String: Int] = [:]

    private var usesHub = false
    private var attention = AttentionWatch()
    private var watchTask: Task<Void, Never>?
    private var remoteTask: Task<Void, Never>?
    /// The newest `seq` the watch reported. It is the daemon's event counter,
    /// so a smaller one means the daemon restarted and every list is stale.
    private var watchSequence = 0
    private var listsByMachine: [String: MachineLists] = [:]
    /// What each daemon itself implements, from its `uhp.capabilities`.
    private var methodsByMachine: [String: Set<String>] = [:]
    /// Methods a daemon answered `unknown method` to since the last capability
    /// read, so the app stops offering what it just learned is missing.
    private var unsupported: [String: Set<String>] = [:]
    private var capabilitiesReadAt: [String: Date] = [:]
    /// Keys for writes whose reply was lost, so the same request resent by the
    /// user dedupes on the host instead of running twice.
    private var pendingKeys: [String: PendingKey] = [:]

    private struct MachineLists {
        var workspaces: [Workspace] = []
        var agents: [Agent] = []
        var tasks: [OrchTask] = []
        var failure: String?
    }

    private struct PendingKey {
        let key: String
        let at: Date
    }

    /// Which of the three lists a refresh covers.
    private struct ListKinds {
        let workspaces: Bool
        let agents: Bool
        let tasks: Bool

        init(topics: Set<String>?) {
            guard let topics else {
                (workspaces, agents, tasks) = (true, true, true)
                return
            }
            workspaces = topics.contains("workspaces")
            agents = topics.contains("agents")
            tasks = topics.contains("tasks")
        }

        var any: Bool { workspaces || agents || tasks }
    }

    var isConnected: Bool { client != nil }
    /// `hub.run`/`hub.agents`/`hub.folders` are operate-only on the hub.
    var canRunAgents: Bool { usesHub && level == "operate" }
    /// Diff review is read at every level; the note writes need control.
    var canViewDiff: Bool { allowedMethods.contains("diff.list") }
    var canReviewDiff: Bool { allowedMethods.contains("diff.note.add") }

    init() {
        deviceId = try? PilusClient.deviceId()
        if let client = PilusClient.resume() {
            self.client = client
            enrolledHost = PilusClient.enrollment()?["endpoint_id"] as? String
            Task {
                await probe()
                await refresh()
            }
        }
    }

    /// Enroll against the host: ticket + the code `pilus-agent` prints.
    func connect(ticket: String, code: String, label: String) async {
        busy = true
        error = nil
        defer { busy = false }
        do {
            let fresh = try await Task.detached {
                let client = try PilusClient(ticket: ticket)
                try client.enroll(code: code, label: label)
                return client
            }.value
            client = fresh
            enrolledHost = PilusClient.enrollment()?["endpoint_id"] as? String
            await probe()
            await refresh()
        } catch {
            self.error = error.localizedDescription
        }
    }

    /// A `pilus://pair/<ticket>?code=…` link (optionally `&name=…`), scanned or opened.
    /// Returns false when the URL is not a pair link.
    @discardableResult
    func connect(pairLink: String) async -> Bool {
        guard let pair = PairLink.parse(pairLink) else { return false }
        await connect(ticket: pair.ticket, code: pair.code ?? "", label: pair.name ?? "iphone")
        return true
    }

    /// Learn what this connector speaks: the hub ALPN when present, the
    /// device's level, the machine catalog, and the agent picker catalog.
    private func probe() async {
        guard let client else { return }
        do {
            let caps: Capabilities = try await Task.detached {
                try client.hubRequest(method: "uhp.capabilities")
            }.value
            usesHub = true
            level = caps.access?.level
            allowedMethods = Set(caps.access?.allowed_methods ?? [])
            methodsByMachine["local"] = Set(caps.methods ?? [])
            capabilitiesReadAt["local"] = Date()
        } catch {
            // A connector without the hub surface only speaks UHP; the app
            // falls back to token-auth requests for everything.
            usesHub = false
            level = nil
            allowedMethods = []
        }
        if usesHub {
            await loadMachines()
            for id in machineOrder where id != "local" {
                await refreshCapabilities(machine: id)
            }
            await loadAgentKinds()
            startWatch()
            startRemotePolling()
        }
    }

    /// Re-read what `machine`'s daemon implements. A saved SSH machine can run
    /// an older Luvus than the paired host, so each is asked separately; the
    /// hub's allowlist still applies on top.
    func refreshCapabilities(machine: String = "local") async {
        guard let client, usesHub else { return }
        let caps: Capabilities? = try? await Task.detached {
            try client.hubRequest(method: "uhp.capabilities", machine: machine)
        }.value
        guard let caps else { return }
        capabilitiesReadAt[machine] = Date()
        unsupported[machine] = nil
        if let methods = caps.methods { methodsByMachine[machine] = Set(methods) }
        if machine == "local" {
            level = caps.access?.level
            allowedMethods = Set(caps.access?.allowed_methods ?? [])
        }
    }

    /// Whether `method` can work on `machine`: this device may call it and that
    /// machine's daemon implements it. Unknown means yes — the call itself
    /// then decides, and `call` learns from an `unknown method` answer.
    func supports(_ method: String, on machine: String = "local") -> Bool {
        if usesHub, !allowedMethods.isEmpty, !allowedMethods.contains(method) { return false }
        if unsupported[machine]?.contains(method) == true { return false }
        if let known = methodsByMachine[machine], !known.isEmpty { return known.contains(method) }
        return true
    }

    /// A daemon said `unknown method`: remember it now, and re-read its
    /// capabilities (at most every 30 seconds) so a Luvus upgrade is noticed.
    private func learnUnsupported(_ method: String, machine: String) {
        unsupported[machine, default: []].insert(method)
        let stale = capabilitiesReadAt[machine].map { Date().timeIntervalSince($0) > 30 } ?? true
        guard stale else { return }
        capabilitiesReadAt[machine] = Date()
        Task {
            let remembered = unsupported[machine] ?? []
            await refreshCapabilities(machine: machine)
            // The re-read is the truth only when it still lacks the method;
            // otherwise the daemon answered `unknown method` for another reason.
            if let known = methodsByMachine[machine], !known.isEmpty {
                unsupported[machine] = remembered.filter { !known.contains($0) }
            }
        }
    }

    /// One request routed through the hub when the connector supports it,
    /// tagged with the owning machine; plain token-auth UHP otherwise.
    func call<Reply: Decodable>(
        method: String,
        params: [String: Any] = [:],
        machine: String = "local"
    ) async throws -> Reply {
        guard let client else { throw PilusError.bridge("not connected") }
        let usesHub = self.usesHub
        do {
            return try await Task.detached {
                if usesHub {
                    return try client.hubRequest(method: method, machine: machine, params: params)
                }
                return try client.request(method: method, params: params)
            }.value
        } catch let error as PilusError where error.isUnknownMethod {
            learnUnsupported(method, machine: machine)
            throw error
        }
    }

    /// Display name for a machine id (`local` is the paired host itself).
    func machineTitle(_ id: String) -> String {
        machines.first(where: { $0.id == id })?.title
            ?? (id == "local" ? hostLabel : nil)
            ?? id
    }

    /// Agent kinds installed on a machine; empty while `hub.agents` is still
    /// loading or the connector predates the catalog. `terminal` is a
    /// pseudo-kind the hub answers with a fresh shell tab — only offered when
    /// the connector advertises it, since an older one forwards it to
    /// `agent.start` and the daemon rejects the unknown kind.
    func kinds(for machine: String) -> [HubAgentKind] {
        let advertised = pseudoKindsByMachine[machine] ?? []
        return (agentKindsByMachine[machine] ?? [])
            + (advertised.contains("terminal") ? [Self.terminalKind] : [])
    }

    private static let terminalKind =
        HubAgentKind(kind: "terminal", label: "Terminal",
                     default_model: nil, models: nil)

    private func loadMachines() async {
        guard let client else { return }
        let reply: HubMachines? = try? await Task.detached {
            try client.hubRequest(method: "hub.machines")
        }.value
        if let reply {
            hostLabel = reply.host
            machines = reply.machines
            var order = ["local"]
            for machine in reply.machines where machine.usable && !order.contains(machine.id) {
                order.append(machine.id)
            }
            machineOrder = order
        }
    }

    /// `hub.agents` answers per machine, so the picker catalog is fetched
    /// once per reachable machine.
    func loadAgentKinds() async {
        guard let client else { return }
        for id in machineOrder {
            let reply: HubAgents? = try? await Task.detached {
                try client.hubRequest(method: "hub.agents", machine: id)
            }.value
            agentKindsByMachine[id] = reply?.agents ?? []
            pseudoKindsByMachine[id] = reply?.pseudo_kinds ?? []
        }
    }

    /// `hub.watch` tells this device that something changed and which lists
    /// to refetch, so the lists follow the host without pull-to-refresh and a
    /// quiet host costs nothing. A reconnect resyncs from scratch, because
    /// signals sent while the stream was down are gone; it backs off while the
    /// host is unreachable.
    private func startWatch() {
        watchTask?.cancel()
        guard let client else { return }
        watchTask = Task.detached(priority: .background) { [weak self] in
            var delay = 1.0
            while !Task.isCancelled {
                do {
                    _ = try client.watchOpen()
                    delay = 1
                    await self?.watchOpened()
                    while !Task.isCancelled {
                        let frame = try client.watchNext()
                        guard let self else { return }
                        guard await self.handleWatch(frame) else { break }
                    }
                } catch {
                    // Fall through to the reconnect below.
                }
                client.watchClose()
                await self?.watchClosed()
                guard !Task.isCancelled else { break }
                try? await Task.sleep(for: .seconds(delay))
                delay = min(delay * 2, 30)
            }
            client.watchClose()
        }
    }

    private func watchOpened() async {
        watchLive = true
        watchSequence = 0
        // The first open follows `probe`, whose caller refreshes next; any
        // later one missed signals while it was down.
        if loaded { await refresh() }
    }

    private func watchClosed() {
        watchLive = false
    }

    /// Act on one watch frame; false when the host is closing the stream.
    private func handleWatch(_ frame: [String: Any]) async -> Bool {
        switch frame["event"] as? String {
        case "closed":
            return false
        case "changed":
            let sequence = frame["seq"] as? Int ?? 0
            if sequence < watchSequence {
                // The counter went backwards: the daemon restarted, so its
                // terminal identities and every list are new.
                watchSequence = sequence
                inventoryCache = [:]
                await refreshCapabilities()
                await refresh()
                return true
            }
            watchSequence = sequence
            for pane in frame["panes"] as? [String] ?? [] {
                outputSignals["local:\(pane)", default: 0] += 1
            }
            let topics = Set(frame["topics"] as? [String] ?? [])
            if ListKinds(topics: topics).any {
                // The watch covers the paired host only; saved SSH machines
                // are refreshed on their own cadence.
                await refresh(topics: topics, machines: ["local"])
            }
            return true
        default:
            return true
        }
    }

    /// Saved SSH machines have no event feed, so their lists are re-read on a
    /// slow timer instead of on every signal from the paired host.
    private func startRemotePolling() {
        remoteTask?.cancel()
        remoteTask = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: .seconds(45))
                guard let self, !Task.isCancelled else { return }
                let remote = self.machineOrder.filter { $0 != "local" }
                if !remote.isEmpty {
                    await self.refresh(topics: ["workspaces", "agents", "tasks"], machines: remote)
                }
            }
        }
    }

    /// Re-read lists and merge every machine's into one flat view — a machine
    /// that fails contributes its error but does not hide the others' data.
    ///
    /// `topics` limits the read to the lists a watch signal named, and
    /// `machines` to the ones that signalled; neither means everything.
    func refresh(topics: Set<String>? = nil, machines only: [String]? = nil) async {
        guard client != nil else { return }
        let wanted = ListKinds(topics: topics)
        guard wanted.any else { return }
        let full = topics == nil
        if full { busy = true }
        defer { if full { busy = false } }
        for id in usesHub ? machineOrder : ["local"] where only?.contains(id) ?? true {
            var lists = listsByMachine[id] ?? MachineLists()
            do {
                let fetched = try await fetchLists(machine: id, wanted: wanted)
                if let replacement = fetched.workspaces {
                    lists.workspaces = replacement.workspaces.map { var row = $0; row.machine = id; return row }
                }
                if let replacement = fetched.agents {
                    lists.agents = replacement.agents.map { var row = $0; row.machine = id; return row }
                    Notifier.handle(attention.observe(machine: id, agents: lists.agents))
                }
                if let replacement = fetched.tasks {
                    lists.tasks = replacement.tasks.map { var row = $0; row.machine = id; return row }
                }
                lists.failure = nil
            } catch {
                lists.failure = "\(machineTitle(id)): \(error.localizedDescription)"
                // A full refresh shows the failure instead of rows it could not
                // confirm; a signal-driven one keeps what it last knew.
                if full { lists.workspaces = []; lists.agents = []; lists.tasks = [] }
            }
            listsByMachine[id] = lists
        }
        let order = usesHub ? machineOrder : ["local"]
        workspaces = order.flatMap { listsByMachine[$0]?.workspaces ?? [] }
        agents = order.flatMap { listsByMachine[$0]?.agents ?? [] }
        tasks = order.flatMap { listsByMachine[$0]?.tasks ?? [] }
        let failures = order.compactMap { listsByMachine[$0]?.failure }
        error = failures.isEmpty ? nil : failures.joined(separator: "\n")
        if !loaded { Task { await Notifier.requestAuthorization() } }
        loaded = true
    }

    /// The requested lists for one machine, in parallel. Rows are stamped with
    /// their machine by the caller so detail/send calls route back to the
    /// right daemon.
    private func fetchLists(
        machine: String,
        wanted: ListKinds
    ) async throws -> (workspaces: WorkspaceList?, agents: AgentList?, tasks: TaskList?) {
        async let workspaces: WorkspaceList? = list("workspace.list", machine: machine, wanted: wanted.workspaces)
        async let agents: AgentList? = list("agent.list", machine: machine, wanted: wanted.agents)
        // Tasks are optional: a daemon without orchestration answers an error.
        async let tasks: TaskList? = try? list("task.list", machine: machine, wanted: wanted.tasks)
        return try await (workspaces, agents, tasks)
    }

    private func list<Reply: Decodable>(_ method: String, machine: String, wanted: Bool) async throws -> Reply? {
        guard wanted else { return nil }
        return try await call(method: method, machine: machine)
    }

    /// `agent.get` + the pane's screen. The styled path is
    /// `terminal.backend.capture` (`ansi: true` keeps the SGR colors); the
    /// plain `pane.read` stays as the fallback for connectors that predate
    /// the terminal backend. The text carries ANSI escapes either way only
    /// when `ansi` comes back true. `lines` bounds how much scrollback the
    /// capture returns; `truncated` in the reply means more history exists,
    /// which is what the detail view's scroll-up pagination grows on.
    ///
    /// A terminal that was replaced since the row was listed is an error, not
    /// a reason to read whatever now sits in that pane.
    func agentDetail(agent: Agent, lines: Int = 300) async throws -> (AgentDetail?, PaneScreen) {
        async let detail: AgentDetail = call(
            method: "agent.get", params: ["target": agent.pane], machine: agent.machine)
        var screen: PaneScreen?
        if supports("terminal.backend.capture", on: agent.machine) {
            do {
                let capture = try await captureScreen(agent: agent, lines: lines)
                screen = PaneScreen(text: capture.text, truncated: capture.truncated ?? false)
            } catch let error as PilusError where error.code == "terminal_replaced" || error.code == "invalid_params" {
                // A widened window the daemon refuses is the scroll-back
                // pagination's signal that the history cap was reached; a plain
                // read would hide it and drop the colors.
                throw error
            } catch {
                // Anything else falls back to the plain read below.
            }
        }
        if screen == nil {
            screen = PaneScreen(
                text: try await paneContent(pane: agent.pane, machine: agent.machine),
                truncated: false)
        }
        return (try? await detail, screen ?? PaneScreen(text: "", truncated: false))
    }

    /// `pane.read` — plain text, the pre-terminal-backend path.
    private func paneContent(pane: String, machine: String) async throws -> String {
        let content: PaneContent = try await call(
            method: "pane.read", params: ["pane": pane], machine: machine)
        return content.text
    }

    /// `terminal.backend.capture` with `ansi: true`, addressed by the runtime
    /// identity triple. A stale `server_generation` (daemon restart) refreshes
    /// the inventory and retries once; the retry re-resolves the same terminal
    /// and never follows the pane to a different one.
    private func captureScreen(agent: Agent, lines: Int) async throws -> TerminalCapture {
        var refresh = false
        while true {
            let identity = try await terminalIdentity(for: agent, refresh: refresh)
            do {
                return try await call(
                    method: "terminal.backend.capture",
                    params: [
                        "server_generation": identity.generation,
                        "terminal_id": identity.terminalID,
                        "pane_id": agent.pane,
                        // Logical lines without the fixed-width screen wrap —
                        // long output wraps to the phone instead of clipping
                        // at the terminal's column count.
                        "mode": "recent_unwrapped",
                        "lines": lines,
                        "ansi": true,
                    ],
                    machine: agent.machine)
            } catch let error as PilusError where !refresh && (error.isStaleRuntime || error.code == "not_found") {
                refresh = true
            }
        }
    }

    /// The identity a terminal write or capture must carry.
    private struct TerminalIdentity {
        let generation: String
        let terminalID: String
    }

    /// Resolve `agent`'s terminal through the inventory. When the row recorded
    /// which terminal it described and the pane now holds another, that is
    /// `terminal_replaced` — after one inventory refresh, in case the cache
    /// was the stale side.
    private func terminalIdentity(for agent: Agent, refresh: Bool = false) async throws -> TerminalIdentity {
        let inventory = try await terminalInventory(machine: agent.machine, refresh: refresh)
        guard let current = inventory.byPane[agent.pane] else {
            throw PilusError(code: "not_terminal", message: "pane \(agent.pane) is not a terminal")
        }
        if let expected = agent.terminal_id, expected != current {
            if !refresh { return try await terminalIdentity(for: agent, refresh: true) }
            throw PilusError(
                code: "terminal_replaced",
                message: "This terminal was replaced on the host. Reopen it from the list.")
        }
        return TerminalIdentity(generation: inventory.generation, terminalID: current)
    }

    private struct InventoryCache {
        let generation: String
        let byPane: [String: String]
        let fetchedAt: Date
    }

    /// Terminal inventories are per machine — each daemon has its own pane
    /// numbering and its own `server_generation`.
    private var inventoryCache: [String: InventoryCache] = [:]

    /// Terminal ids by pane id plus the server's generation, cached briefly —
    /// the generation only changes on a daemon restart and the detail view
    /// polls every few seconds.
    private func terminalInventory(machine: String, refresh: Bool = false) async throws -> InventoryCache {
        if !refresh, let cache = inventoryCache[machine],
           Date().timeIntervalSince(cache.fetchedAt) < 30 {
            return cache
        }
        let reply: TerminalInventory = try await call(
            method: "terminal.backend.inventory", machine: machine)
        let cache = InventoryCache(
            generation: reply.server_generation,
            byPane: Dictionary(
                reply.terminals.map { ($0.pane_id, $0.terminal_id) },
                uniquingKeysWith: { first, _ in first }),
            fetchedAt: Date())
        inventoryCache[machine] = cache
        return cache
    }

    /// Reply into the agent's session with `agent.prompt`, fenced to the
    /// terminal the row described so a restarted session is refused instead of
    /// answered.
    ///
    /// A prompt is never resent on its own: after a dropped connection it may
    /// already be in the agent's composer, so the error says so and the caller
    /// keeps the text. An agent sitting at a startup, sign-in, or approval
    /// screen refuses with `agent_not_ready`; callers fall back to
    /// `answer(agent:text:)` when `agent.keys` is available. Hold-to-type
    /// guidance is included only when a terminal input action is available.
    func sendPrompt(agent: Agent, text: String) async throws {
        var params: [String: Any] = ["target": agent.pane, "text": text]
        if let terminal = agent.terminal_id { params["terminal_id"] = terminal }
        do {
            let _: EmptyReply = try await call(
                method: "agent.prompt", params: params, machine: agent.machine)
        } catch let error as PilusError {
            switch error.code {
            case "content_revision_conflict":
                await refresh(topics: ["agents"], machines: [agent.machine])
                throw PilusError(
                    code: error.code,
                    message: "This session was restarted on the host. Reopen it from the list.")
            case "agent_not_ready":
                let terminalInputAvailable = [
                    TerminalInput.typeLiteral(""),
                    .pasteText(""),
                    .submitText(""),
                ].contains { canSend($0, to: agent) }
                throw PilusError(
                    code: error.code,
                    message: error.message + (terminalInputAvailable
                        ? " Hold the send button to type it into the terminal anyway."
                        : ""))
            default:
                throw error
            }
        }
    }

    /// Type one scalar at a time through `agent.keys`, then submit the reply.
    func answer(agent: Agent, text: String) async throws {
        guard supports("agent.keys", on: agent.machine) else {
            throw PilusError(
                code: "unsupported",
                message: "This host's Luvus can't send keys to agents.")
        }
        var keys = text.unicodeScalars.map { scalar in
            CharacterSet.controlCharacters.contains(scalar) ? " " : String(scalar)
        }
        keys.append("enter")
        for start in stride(from: 0, to: keys.count, by: 400) {
            let end = min(start + 400, keys.count)
            let _: EmptyReply = try await call(
                method: "agent.keys",
                params: ["target": agent.pane, "keys": Array(keys[start..<end])],
                machine: agent.machine)
        }
    }

    /// One literal input to a terminal, through the identity-fenced
    /// `terminal.backend` methods.
    enum TerminalInput {
        /// UTF-8 as typed, no Enter.
        case typeLiteral(String)
        /// Bracketed paste, no Enter.
        case pasteText(String)
        /// Paste and Enter as one queued action.
        case submitText(String)
        /// One logical key, such as `enter`.
        case key(String)

        var method: String {
            switch self {
            case .typeLiteral: "terminal.backend.type_literal"
            case .pasteText: "terminal.backend.paste_text"
            case .submitText: "terminal.backend.submit_text"
            case .key: "terminal.backend.send_key"
            }
        }

        var params: [String: Any] {
            switch self {
            case .typeLiteral(let text), .pasteText(let text), .submitText(let text): ["text": text]
            case .key(let key): ["key": key]
            }
        }
    }

    /// Set or clear (empty) the alias the host shows for `agent`'s pane. The
    /// daemon takes `[a-z][a-z0-9_-]{0,31}` and announces `pane.renamed`, which
    /// the watch turns into an agent-list refresh.
    func renameAgent(_ agent: Agent, to name: String) async throws {
        do {
            let _: EmptyReply = try await call(
                method: "pane.rename", params: ["pane": agent.pane, "name": name], machine: agent.machine)
        } catch let error as PilusError where error.isUnconfirmed {
            await refresh(topics: ["agents"], machines: [agent.machine])
            throw error
        }
        await refresh(topics: ["agents"], machines: [agent.machine])
    }

    /// `input` folded into the shape the daemon accepts: lowercase, spaces
    /// and dashes as `-`, everything else outside `a-z 0-9 _ -` dropped, a
    /// letter first, at most 32 characters. Empty stays empty, which clears
    /// the alias.
    static func normalizedAlias(_ input: String) -> String {
        var folded = ""
        for character in input.lowercased() {
            if character.isWhitespace || character == "-" {
                folded.append("-")
            } else if character == "_" || (character.isASCII && (character.isLetter || character.isNumber)) {
                folded.append(character)
            }
        }
        // The first character must be a letter.
        let start = folded.firstIndex(where: { $0.isLetter }) ?? folded.endIndex
        return String(folded[start...].prefix(32))
    }

    /// Whether `input` can be sent to `agent`'s machine.
    func canSend(_ input: TerminalInput, to agent: Agent) -> Bool {
        supports(input.method, on: agent.machine)
    }

    /// Send `input` to `agent`'s terminal. The daemon rejects a stale
    /// generation or a replaced terminal before queueing anything, which is
    /// the only case retried here, after re-reading the identity. Every other
    /// failure surfaces: a dropped connection leaves the input's fate unknown.
    func terminalInput(agent: Agent, _ input: TerminalInput) async throws {
        guard supports(input.method, on: agent.machine) else {
            throw PilusError(code: "unsupported", message: "This host's Luvus can't take that input.")
        }
        var refresh = false
        while true {
            let identity = try await terminalIdentity(for: agent, refresh: refresh)
            var params = input.params
            params["server_generation"] = identity.generation
            params["terminal_id"] = identity.terminalID
            params["pane_id"] = agent.pane
            do {
                let _: EmptyReply = try await call(
                    method: input.method, params: params, machine: agent.machine)
                return
            } catch let error as PilusError where !refresh && error.isStaleRuntime {
                refresh = true
            }
        }
    }

    /// Named keys straight to the pane — the shortcut bar's escape, ctrl+C/D,
    /// arrows, and tab. Enter goes through the identity-fenced `send_key` when
    /// the host has it. The rest use `agent.keys`, which has no fence a phone
    /// can use: its content-revision guard rejects the moment the screen
    /// moves, which would make ctrl+C unusable while an agent streams.
    func sendKeys(agent: Agent, keys: [String]) async throws {
        if keys == ["ENTER"], canSend(.key("enter"), to: agent) {
            try await terminalInput(agent: agent, .key("enter"))
            return
        }
        let _: EmptyReply = try await call(
            method: "agent.keys", params: ["target": agent.pane, "keys": keys],
            machine: agent.machine)
    }

    /// The key a repeat of the same write reuses, so a request whose reply was
    /// lost dedupes on the host. Kept for under the hub's replay window.
    private func replayKey(for signature: String) -> PendingKey {
        let now = Date()
        pendingKeys = pendingKeys.filter { now.timeIntervalSince($0.value.at) < 540 }
        if let existing = pendingKeys[signature] { return existing }
        let fresh = PendingKey(key: UUID().uuidString, at: now)
        pendingKeys[signature] = fresh
        return fresh
    }

    /// `hub.run`: open the workspace, start a new agent in a new tab, and
    /// queue its prompt after startup — one audited request on the connector.
    /// Operate-level only.
    ///
    /// The request carries an `idempotency_key`. If the reply is lost, tapping
    /// again sends the same key and the host answers with the first outcome
    /// instead of starting a second agent.
    @discardableResult
    func runAgent(
        agent: String,
        path: String,
        prompt: String,
        workspaceID: String? = nil,
        model: String? = nil,
        effort: String? = nil,
        machine: String = "local"
    ) async throws -> HubRunResult {
        var params: [String: Any] = ["agent": agent, "path": path, "prompt": prompt]
        if let workspaceID { params["workspace_id"] = workspaceID }
        if let model { params["model"] = model }
        if let effort { params["effort"] = effort }
        let signature = ["run", machine, agent, path, workspaceID ?? "", model ?? "", effort ?? "", prompt]
            .joined(separator: "\u{1f}")
        params["idempotency_key"] = replayKey(for: signature).key
        do {
            let result: HubRunResult = try await call(method: "hub.run", params: params, machine: machine)
            pendingKeys[signature] = nil
            return result
        } catch let error as PilusError {
            if !error.isUnconfirmed { pendingKeys[signature] = nil }
            throw error
        }
    }

    /// `workspace.open`: open `path` as a workspace on `machine`, or focus it
    /// when it is already open there. Operate-level.
    ///
    /// When the reply is lost the workspace list says whether it landed, so a
    /// retry is never needed.
    func openWorkspace(path: String, machine: String) async throws {
        do {
            let _: EmptyReply = try await call(
                method: "workspace.open", params: ["path": path], machine: machine)
        } catch let error as PilusError where error.isUnconfirmed {
            let listed: WorkspaceList? = try? await call(method: "workspace.list", machine: machine)
            let wanted = path.trimmingCharacters(in: CharacterSet(charactersIn: "/"))
            let opened = listed?.workspaces.contains {
                ($0.cwd ?? "").trimmingCharacters(in: CharacterSet(charactersIn: "/")) == wanted
            } ?? false
            if !opened { throw error }
        }
    }

    /// Resolve a task to the pane its workspace worker runs in: the task
    /// carries `workspace_worker.tab_id`, and `tab.get` answers the panes in
    /// that tab. Surfaced as a synthetic agent so the detail view can show
    /// the worker's screen.
    func taskAgent(_ task: OrchTask) async throws -> Agent? {
        guard let worker = task.workspace_worker,
              let workspaceID = worker.workspace_id,
              let tabID = worker.tab_id else { return nil }
        let tab: TabDetail = try await call(
            method: "tab.get",
            params: ["workspace_id": workspaceID, "tab_id": tabID],
            machine: task.machine)
        guard let pane = tab.focus ?? tab.panes?.first else { return nil }
        return Agent(
            pane: pane,
            agent: "bot",
            name: tab.name ?? task.title,
            agent_session_title: nil,
            status: task.status,
            workspace_id: workspaceID,
            workspace_name: nil,
            cwd: worker.root,
            branch: nil,
            focused: true,
            machine: task.machine)
    }

    /// Child folders under `path` on `machine`, for the workspace path
    /// pickers.
    func folders(path: String = "~", machine: String = "local") async throws -> HubFolders {
        guard let client else { throw PilusError.bridge("not connected") }
        return try await Task.detached {
            try client.hubRequest(method: "hub.folders", machine: machine, params: ["path": path])
        }.value
    }

    /// Spawn an ORCH workspace worker: `automation.create` + `automation.run`
    /// on the machine the workspace lives on.
    ///
    /// Both calls carry an `idempotency_key` (the daemon's own mechanism for
    /// these two methods), and the trigger time is fixed with the key because
    /// the daemon answers a reused key with different input as a conflict. A
    /// repeat after a lost reply therefore gets the original automation and its
    /// original run.
    func createTask(workspaceID: String, agent: String, title: String, prompt: String) async throws {
        let machine = workspaces.first(where: { $0.workspace_id == workspaceID })?.machine ?? "local"
        let signature = ["task", machine, workspaceID, agent, title, prompt].joined(separator: "\u{1f}")
        let pending = replayKey(for: signature)
        var created = false
        do {
            let automation: AutomationCreated = try await call(method: "automation.create", params: [
                "name": title,
                "trigger": ["kind": "once", "at_utc": Int(pending.at.timeIntervalSince1970) + 86_400],
                "target": ["kind": "new_worker"],
                "task": [
                    "title": title,
                    "prompt": prompt,
                    "agent_id": agent,
                    "workspace_id": workspaceID,
                    "mode": "workspace",
                ],
                "idempotency_key": pending.key,
            ], machine: machine)
            created = true
            let _: EmptyReply = try await call(
                method: "automation.run",
                params: ["id": automation.automation.id, "idempotency_key": pending.key + ".run"],
                machine: machine)
            pendingKeys[signature] = nil
        } catch let error as PilusError {
            // Once the automation exists, a repeat reuses it under the same
            // key rather than creating a second.
            if !created && !error.isUnconfirmed { pendingKeys[signature] = nil }
            throw error
        }
    }

    func disconnect() {
        watchTask?.cancel()
        watchTask = nil
        remoteTask?.cancel()
        remoteTask = nil
        watchLive = false
        watchSequence = 0
        outputSignals = [:]
        listsByMachine = [:]
        methodsByMachine = [:]
        unsupported = [:]
        capabilitiesReadAt = [:]
        pendingKeys = [:]
        PilusClient.forget()
        client = nil
        enrolledHost = nil
        hostLabel = nil
        machines = []
        machineOrder = ["local"]
        agentKindsByMachine = [:]
        pseudoKindsByMachine = [:]
        inventoryCache = [:]
        workspaces = []
        agents = []
        tasks = []
        attention.reset()
        Notifier.withdrawAll()
        level = nil
        allowedMethods = []
        usesHub = false
        error = nil
    }
}

/// `pilus://pair/<ticket>?code=…&name=…` — what the host's QR and printed
/// link carry.
struct PairLink {
    let ticket: String
    let code: String?
    let name: String?

    static func parse(_ raw: String) -> PairLink? {
        let trimmed = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        guard trimmed.hasPrefix("pilus://pair/"),
              let components = URLComponents(string: trimmed) else { return nil }
        let ticket = components.path.trimmingCharacters(in: CharacterSet(charactersIn: "/"))
        guard !ticket.isEmpty else { return nil }
        let items = components.queryItems ?? []
        return PairLink(
            ticket: ticket,
            code: items.first(where: { $0.name == "code" })?.value,
            name: items.first(where: { $0.name == "name" })?.value
        )
    }
}
