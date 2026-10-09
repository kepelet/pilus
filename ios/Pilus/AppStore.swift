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

    private var usesHub = false
    private var watchTask: Task<Void, Never>?

    var isConnected: Bool { client != nil }
    /// `hub.run`/`hub.agents`/`hub.folders` are operate-only on the hub.
    var canRunAgents: Bool { usesHub && level == "operate" }

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

    /// A `pilus://pair/<ticket>?code=…&name=…` link, scanned or opened.
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
        } catch {
            // A connector without the hub surface only speaks UHP; the app
            // falls back to token-auth requests for everything.
            usesHub = false
            level = nil
        }
        if usesHub {
            await loadMachines()
            await loadAgentKinds()
            startWatch()
        }
    }

    /// One request routed through the hub when the connector supports it,
    /// tagged with the owning machine; plain token-auth UHP otherwise.
    private func call<Reply: Decodable>(
        method: String,
        params: [String: Any] = [:],
        machine: String = "local"
    ) async throws -> Reply {
        guard let client else { throw PilusError.bridge("not connected") }
        let usesHub = self.usesHub
        return try await Task.detached {
            if usesHub {
                return try client.hubRequest(method: method, machine: machine, params: params)
            }
            return try client.request(method: method, params: params)
        }.value
    }

    /// Display name for a machine id (`local` is the paired host itself).
    func machineTitle(_ id: String) -> String {
        machines.first(where: { $0.id == id })?.title
            ?? (id == "local" ? hostLabel : nil)
            ?? id
    }

    /// Agent kinds installed on a machine; empty while `hub.agents` is still
    /// loading or the connector predates the catalog. `terminal` is a
    /// pseudo-kind the hub answers with a fresh shell tab.
    func kinds(for machine: String) -> [HubAgentKind] {
        (agentKindsByMachine[machine] ?? []) + [Self.terminalKind]
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
            agentKindsByMachine[id] = (try? await Task.detached {
                let reply: HubAgents = try client.hubRequest(method: "hub.agents", machine: id)
                return reply.agents
            }.value) ?? []
        }
    }

    /// `hub.watch` relays this machine's change signals; each one triggers a
    /// refresh so the list follows the host without pull-to-refresh.
    /// Reconnects with a short backoff when the stream ends.
    private func startWatch() {
        watchTask?.cancel()
        guard let client else { return }
        watchTask = Task.detached(priority: .background) { [weak self] in
            while !Task.isCancelled {
                do {
                    _ = try client.watchOpen()
                    while !Task.isCancelled {
                        _ = try client.watchNext()
                        guard let self, !Task.isCancelled else { return }
                        await self.refresh()
                    }
                } catch {
                    client.watchClose()
                    try? await Task.sleep(for: .seconds(3))
                }
            }
            client.watchClose()
        }
    }

    /// Merge every machine's lists into one flat view — a machine that fails
    /// contributes its error but does not hide the others' data.
    func refresh() async {
        guard client != nil else { return }
        busy = true
        defer { busy = false }
        var mergedWorkspaces: [Workspace] = []
        var mergedAgents: [Agent] = []
        var mergedTasks: [OrchTask] = []
        var failures: [String] = []
        for id in usesHub ? machineOrder : ["local"] {
            do {
                let lists = try await fetchLists(machine: id)
                mergedWorkspaces += lists.0
                mergedAgents += lists.1
                mergedTasks += lists.2
            } catch {
                failures.append("\(machineTitle(id)): \(error.localizedDescription)")
            }
        }
        workspaces = mergedWorkspaces
        agents = mergedAgents
        tasks = mergedTasks
        error = failures.isEmpty ? nil : failures.joined(separator: "\n")
        loaded = true
    }

    /// `workspace.list` + `agent.list` + `task.list` for one machine, each
    /// element stamped with its machine so detail/send calls route back to
    /// the right daemon.
    private func fetchLists(machine: String) async throws -> ([Workspace], [Agent], [OrchTask]) {
        async let fetchedWorkspaces: WorkspaceList = call(method: "workspace.list", machine: machine)
        async let fetchedAgents: AgentList = call(method: "agent.list", machine: machine)
        async let fetchedTasks: TaskList = call(method: "task.list", machine: machine)
        var workspaceList = try await fetchedWorkspaces.workspaces
        var agentList = try await fetchedAgents.agents
        var taskList = (try? await fetchedTasks.tasks) ?? []
        for i in workspaceList.indices { workspaceList[i].machine = machine }
        for i in agentList.indices { agentList[i].machine = machine }
        for i in taskList.indices { taskList[i].machine = machine }
        return (workspaceList, agentList, taskList)
    }

    /// `agent.get` + the pane's screen. The styled path is
    /// `terminal.backend.capture` (`ansi: true` keeps the SGR colors); the
    /// plain `pane.read` stays as the fallback for connectors that predate
    /// the terminal backend. The text carries ANSI escapes either way only
    /// when `ansi` comes back true. `lines` bounds how much scrollback the
    /// capture returns; `truncated` in the reply means more history exists,
    /// which is what the detail view's scroll-up pagination grows on.
    func agentDetail(agent: Agent, lines: Int = 300) async throws -> (AgentDetail?, PaneScreen) {
        async let detail: AgentDetail = call(
            method: "agent.get", params: ["target": agent.pane], machine: agent.machine)
        let screen: PaneScreen
        if let capture = try? await captureScreen(pane: agent.pane, machine: agent.machine, lines: lines) {
            screen = PaneScreen(text: capture.text, truncated: capture.truncated ?? false)
        } else {
            screen = PaneScreen(
                text: try await paneContent(pane: agent.pane, machine: agent.machine),
                truncated: false)
        }
        return (try? await detail, screen)
    }

    /// `pane.read` — plain text, the pre-terminal-backend path.
    private func paneContent(pane: String, machine: String) async throws -> String {
        let content: PaneContent = try await call(
            method: "pane.read", params: ["pane": pane], machine: machine)
        return content.text
    }

    /// `terminal.backend.capture` with `ansi: true`, resolved through a
    /// cached `terminal.backend.inventory` for the runtime identity triple.
    /// A stale `server_generation` (daemon restart) invalidates the cache and
    /// retries once before the caller falls back to `pane.read`.
    private func captureScreen(pane: String, machine: String, lines: Int) async throws -> TerminalCapture {
        for refresh in [false, true] {
            let inventory = try await terminalInventory(machine: machine, refresh: refresh)
            guard let terminalID = inventory.byPane[pane] else {
                throw PilusError.bridge("pane \(pane) is not a terminal")
            }
            do {
                let capture: TerminalCapture = try await call(
                    method: "terminal.backend.capture",
                    params: [
                        "server_generation": inventory.generation,
                        "terminal_id": terminalID,
                        "pane_id": pane,
                        // Logical lines without the fixed-width screen wrap —
                        // long output wraps to the phone instead of clipping
                        // at the terminal's column count.
                        "mode": "recent_unwrapped",
                        "lines": lines,
                        "ansi": true,
                    ],
                    machine: machine)
                return capture
            } catch {
                if refresh { throw error }
            }
        }
        throw PilusError.bridge("terminal capture failed")
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

    /// Reply into the agent's session: `agent.prompt`, or per-key `agent.keys`
    /// when the agent has no prompt-ready composer.
    func sendPrompt(pane: String, machine: String, text: String) async throws {
        do {
            let _: EmptyReply = try await call(
                method: "agent.prompt", params: ["target": pane, "text": text],
                machine: machine)
        } catch {
            let keys = text.map(String.init) + ["ENTER"]
            let _: EmptyReply = try await call(
                method: "agent.keys", params: ["target": pane, "keys": keys],
                machine: machine)
        }
    }

    /// Named keys straight to the pane — the shortcut bar's escape, ctrl+C/D,
    /// arrows, and tab.
    func sendKeys(pane: String, machine: String, keys: [String]) async throws {
        let _: EmptyReply = try await call(
            method: "agent.keys", params: ["target": pane, "keys": keys],
            machine: machine)
    }

    /// `hub.run`: open (or reuse) a workspace, start the agent there, prompt
    /// it — one audited request on the connector. Operate-level only.
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
        return try await call(method: "hub.run", params: params, machine: machine)
    }

    /// `workspace.open`: open `path` as a workspace on `machine`, or focus it
    /// when it is already open there. Operate-level.
    func openWorkspace(path: String, machine: String) async throws {
        let _: EmptyReply = try await call(
            method: "workspace.open", params: ["path": path], machine: machine)
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
    func createTask(workspaceID: String, agent: String, title: String, prompt: String) async throws {
        let machine = workspaces.first(where: { $0.workspace_id == workspaceID })?.machine ?? "local"
        let created: AutomationCreated = try await call(method: "automation.create", params: [
            "name": title,
            "trigger": ["kind": "once", "at_utc": Int(Date.now.timeIntervalSince1970) + 86_400],
            "target": ["kind": "new_worker"],
            "task": [
                "title": title,
                "prompt": prompt,
                "agent_id": agent,
                "workspace_id": workspaceID,
                "mode": "workspace",
            ],
        ], machine: machine)
        let _: EmptyReply = try await call(
            method: "automation.run", params: ["id": created.automation.id], machine: machine)
    }

    func disconnect() {
        watchTask?.cancel()
        watchTask = nil
        PilusClient.forget()
        client = nil
        enrolledHost = nil
        hostLabel = nil
        machines = []
        machineOrder = ["local"]
        agentKindsByMachine = [:]
        inventoryCache = [:]
        workspaces = []
        agents = []
        tasks = []
        level = nil
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
