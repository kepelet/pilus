import Foundation

// Shapes mirrored from the daemon's UHP replies and the connector's hub
// methods (pilus-agent's hub.rs). Optional fields stay optional: the daemon
// is free to add or drop keys between revisions.

struct WorkspaceList: Decodable {
    let workspaces: [Workspace]
    let revision: Int?
}

struct Workspace: Decodable, Identifiable {
    let workspace_id: String
    let workspace: String?
    let name: String?
    let cwd: String?
    let terminal_cwd: String?
    let active: Bool?
    let pinned: Bool?
    let tabs: Int?
    let display_position: String?

    /// Which hub machine the workspace lives on, stamped client-side after
    /// decode — every daemon call for it carries this as the machine tag.
    var machine = "local"

    var id: String { workspace_id }
    var title: String { name ?? workspace_id }
}

struct AgentList: Decodable {
    let agents: [Agent]
    let revision: Int?
}

struct Agent: Decodable, Identifiable, Hashable {
    let pane: String
    let agent: String?
    let name: String?
    let agent_session_title: String?
    let status: String?
    let workspace_id: String?
    let workspace_name: String?
    let cwd: String?
    let branch: String?
    let focused: Bool?

    /// Which hub machine the pane lives on, stamped client-side after decode.
    /// Pane ids are only unique per daemon, so the identity tags it too.
    var machine = "local"
    /// The PTY this row described. A pane id can outlive its terminal, so
    /// writes carry this and the daemon refuses a replaced one instead of
    /// typing into whatever now sits in the pane.
    var terminal_id: String?

    var id: String { "\(machine):\(pane)" }
    /// The session's own title when the agent reports one, else the pane's
    /// alias, else the kind.
    var title: String {
        if let t = agent_session_title, !t.isEmpty { return t }
        return name ?? agent ?? pane
    }
    /// The agent kind driving the row's icon — `claude`, `pi`, …
    var kind: String { agent ?? "" }
}

// `var machine = "local"` would be decoded as a required key — Codable
// synthesis ignores property defaults — so the three list models decode
// `machine` via `decodeIfPresent` in extensions instead. Keeping them in
// extensions preserves the memberwise initializers the views use.
extension Workspace {
    private enum CodingKeys: String, CodingKey {
        case workspace_id, workspace, name, cwd, terminal_cwd
        case active, pinned, tabs, display_position, machine
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        workspace_id = try c.decode(String.self, forKey: .workspace_id)
        workspace = try c.decodeIfPresent(String.self, forKey: .workspace)
        name = try c.decodeIfPresent(String.self, forKey: .name)
        cwd = try c.decodeIfPresent(String.self, forKey: .cwd)
        terminal_cwd = try c.decodeIfPresent(String.self, forKey: .terminal_cwd)
        active = try c.decodeIfPresent(Bool.self, forKey: .active)
        pinned = try c.decodeIfPresent(Bool.self, forKey: .pinned)
        tabs = try c.decodeIfPresent(Int.self, forKey: .tabs)
        display_position = try c.decodeIfPresent(String.self, forKey: .display_position)
        machine = try c.decodeIfPresent(String.self, forKey: .machine) ?? "local"
    }
}

extension Agent {
    private enum CodingKeys: String, CodingKey {
        case pane, agent, name, agent_session_title, status
        case workspace_id, workspace_name, cwd, branch, focused, machine, terminal_id
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        pane = try c.decode(String.self, forKey: .pane)
        agent = try c.decodeIfPresent(String.self, forKey: .agent)
        name = try c.decodeIfPresent(String.self, forKey: .name)
        agent_session_title = try c.decodeIfPresent(String.self, forKey: .agent_session_title)
        status = try c.decodeIfPresent(String.self, forKey: .status)
        workspace_id = try c.decodeIfPresent(String.self, forKey: .workspace_id)
        workspace_name = try c.decodeIfPresent(String.self, forKey: .workspace_name)
        cwd = try c.decodeIfPresent(String.self, forKey: .cwd)
        branch = try c.decodeIfPresent(String.self, forKey: .branch)
        focused = try c.decodeIfPresent(Bool.self, forKey: .focused)
        machine = try c.decodeIfPresent(String.self, forKey: .machine) ?? "local"
        terminal_id = try c.decodeIfPresent(String.self, forKey: .terminal_id)
    }
}

extension OrchTask {
    private enum CodingKeys: String, CodingKey {
        case id, title, status, workspace_worker, updated, machine
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(String.self, forKey: .id)
        title = try c.decodeIfPresent(String.self, forKey: .title)
        status = try c.decodeIfPresent(String.self, forKey: .status)
        workspace_worker = try c.decodeIfPresent(WorkspaceWorker.self, forKey: .workspace_worker)
        updated = try c.decodeIfPresent(Int.self, forKey: .updated)
        machine = try c.decodeIfPresent(String.self, forKey: .machine) ?? "local"
    }
}

/// `agent.get` reply — richer detail for one agent pane.
struct AgentDetail: Decodable {
    let pane: String
    let agent: String?
    let name: String?
    let status: String?
    let state_source: String?
    let cwd: String?
    let session: String?
    let agent_session_title: String?
    let authority: String?
}

/// `pane.read` reply — the pane's visible screen as plain text.
struct PaneContent: Decodable {
    let text: String
}

/// A pane's rendered screen plus whether older scrollback exists than the
/// `lines` window returned — `truncated` drives "load older" pagination.
struct PaneScreen {
    let text: String
    let truncated: Bool
}

/// `terminal.backend.inventory` reply — the runtime identity triple
/// (`server_generation` + `terminal_id` + `pane_id`) every terminal method
/// keys on.
struct TerminalInventory: Decodable {
    let server_generation: String
    let terminals: [Entry]

    struct Entry: Decodable {
        let pane_id: String
        let terminal_id: String
    }
}

/// `terminal.backend.capture` reply — with `ansi: true`, `text` carries the
/// screen's SGR escape sequences, so the client can render its real colors.
struct TerminalCapture: Decodable {
    let text: String
    let ansi: Bool?
    let truncated: Bool?
}

/// `task.list` reply — ORCH tasks (workspace workers live here, not in
/// `agent.list`, so spawned agents surface through this list).
struct TaskList: Decodable {
    let tasks: [OrchTask]
    let revision: Int?
}

struct OrchTask: Decodable, Identifiable {
    let id: String
    let title: String?
    let status: String?
    let workspace_worker: WorkspaceWorker?
    let updated: Int?
    /// Which hub machine the task runs on, stamped client-side. Task ids are
    /// only unique per daemon, so `uiID` is the collision-safe identity.
    var machine = "local"

    var uiID: String { "\(machine):\(id)" }

    struct WorkspaceWorker: Decodable {
        let workspace_id: String?
        let tab_id: String?
        let root: String?
    }
}

/// `tab.get` reply — resolves a tab (e.g. a task's `workspace_worker.tab_id`)
/// to the pane ids it shows so a row can deep-link into a terminal view.
struct TabDetail: Decodable {
    let panes: [String]?
    let focus: String?
    let name: String?
}

/// `automation.create` reply — only the id is needed to fire `automation.run`.
struct AutomationCreated: Decodable {
    let automation: AutomationRef
    struct AutomationRef: Decodable {
        let id: String
    }
}

/// `hub.machines` reply — the paired host plus its saved SSH machines.
struct HubMachines: Decodable {
    let host: String?
    let machines: [Machine]
}

struct Machine: Decodable, Identifiable {
    let id: String
    let label: String?
    let kind: String?
    let destination: String?
    let enabled: Bool?
    let ready: Bool?
    let error: String?

    var title: String { label ?? id }
    var usable: Bool { enabled == true && ready == true }
}

/// `hub.agents` reply — the agent kinds installed on a machine, with the
/// model catalog a picker offers.
struct HubAgents: Decodable {
    let agents: [HubAgentKind]
    /// Kinds `hub.run` accepts beyond real agents (`terminal` opens a shell
    /// tab); absent on connectors that predate pseudo-kinds.
    let pseudo_kinds: [String]?
}

struct HubAgentKind: Decodable, Identifiable {
    let kind: String
    let label: String?
    let default_model: String?
    let models: [HubAgentModel]?

    var id: String { kind }
    var title: String { label ?? kind }
    var defaultModel: String? {
        default_model ?? models?.first(where: { $0.is_default == true })?.name
    }
}

struct HubAgentModel: Decodable, Identifiable, Hashable {
    let name: String
    let efforts: [String]?
    let default_effort: String?
    let is_default: Bool?

    var id: String { name }
}

/// `hub.folders` reply — child directories for the workspace path picker.
struct HubFolders: Decodable {
    let root: String
    let parent: String?
    let folders: [Folder]
}

struct Folder: Decodable, Identifiable {
    let name: String
    let path: String
    let git: Bool?

    var id: String { path }
}

/// `hub.run` reply — what the launch produced.
struct HubRunResult: Decodable {
    let started: Bool?
    let name: String?
    let pane: String?
    let prompt: String?
}

/// `uhp.capabilities` through the hub: the device's own effective access.
struct Capabilities: Decodable {
    struct Access: Decodable {
        let level: String?
        let allowed_methods: [String]?
    }
    let access: Access?
    /// What the daemon itself implements — the server catalog, not what this
    /// device may call. A saved SSH machine can run an older Luvus.
    let methods: [String]?
}

/// Write-method replies (`{"type":"ok","revision":…}`) whose body we ignore.
struct EmptyReply: Decodable {}

/// One agent session from `hub.sessions` — read from the agent's own
/// transcript on the machine, like the desktop's Search and Home pages.
struct SessionSummary: Codable, Identifiable, Hashable {
    let agent: String
    let session_id: String
    let title: String
    let cwd: String?
    let branch: String?
    let started: Int?
    let updated: Int?
    let path: String

    /// Stamped client-side after decode, like `Agent.machine`.
    var machine = "local"

    var id: String { "\(machine):\(path)" }
    var kind: String { agent }

    private enum CodingKeys: String, CodingKey {
        case agent, session_id, title, cwd, branch, started, updated, path, machine
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        agent = try c.decode(String.self, forKey: .agent)
        session_id = try c.decode(String.self, forKey: .session_id)
        title = try c.decode(String.self, forKey: .title)
        cwd = try c.decodeIfPresent(String.self, forKey: .cwd)
        branch = try c.decodeIfPresent(String.self, forKey: .branch)
        started = try c.decodeIfPresent(Int.self, forKey: .started)
        updated = try c.decodeIfPresent(Int.self, forKey: .updated)
        path = try c.decode(String.self, forKey: .path)
        // Hubs never send it; the on-device cache does.
        machine = try c.decodeIfPresent(String.self, forKey: .machine) ?? "local"
    }
}

struct SessionList: Decodable {
    let sessions: [SessionSummary]
}

/// One thing said or done in a session, as `hub.transcript` folds it.
struct TranscriptEntry: Codable, Hashable {
    let type: String
    let text: String?
    let name: String?
    let summary: String?
    let output: String?
    let failed: Bool?
    let at: Int?
}

struct TranscriptReply: Codable {
    let entries: [TranscriptEntry]
    let earlier: Bool?
}
