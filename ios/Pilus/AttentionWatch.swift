import Foundation

/// Spots the moment an agent starts waiting on the user.
///
/// Each refresh hands over one machine's `agent.list`; the watch folds it
/// into transitions: a pane that was not `blocked` and now is, or one that
/// stops being `blocked`. It touches no UI, so the rules stay testable and the
/// store decides how to surface them. Mirrors `gui/attention.rs` on desktop.
struct AttentionWatch {
    enum Event: Equatable {
        case blocked(Blocked)
        /// The agent no longer waits, so its notification is stale.
        case cleared(tag: String)
    }

    struct Blocked: Equatable {
        /// Stable per pane, so a repeat replaces the earlier notification.
        /// The host's push alerts use the same id (`push.rs`), which is how a
        /// delivered push is found again to take it down.
        let tag: String
        let machine: String
        /// The agent kind's display name, such as `Claude`.
        let agent: String
        /// The session's own title, else the pane's alias, else the workspace.
        let session: String
    }

    /// Last seen status per pane, per machine.
    private var seen: [String: [String: String]] = [:]
    /// Machines that have reported once. An agent already blocked when the app
    /// connects, or a machine first appears, is old news, not an alert.
    private var seeded: Set<String> = []

    mutating func observe(machine: String, agents: [Agent]) -> [Event] {
        let isSeeded = !seeded.insert(machine).inserted
        let previous = seen[machine] ?? [:]
        var current: [String: String] = [:]
        var events: [Event] = []
        for agent in agents {
            let tag = Self.tag(machine: machine, pane: agent.pane)
            let wasBlocked = previous[agent.pane] == "blocked"
            let isBlocked = agent.status == "blocked"
            if isSeeded && isBlocked && !wasBlocked {
                events.append(.blocked(Blocked(
                    tag: tag,
                    machine: machine,
                    agent: Self.label(agent.kind),
                    session: [agent.agent_session_title, agent.name, agent.workspace_name]
                        .compactMap { $0 }
                        .first { !$0.isEmpty } ?? ""
                )))
            } else if wasBlocked && !isBlocked {
                events.append(.cleared(tag: tag))
            }
            current[agent.pane] = agent.status ?? ""
        }
        // A pane that vanished while blocked (the agent exited) leaves its
        // banner behind unless it is cleared too.
        for (pane, status) in previous where status == "blocked" && current[pane] == nil {
            events.append(.cleared(tag: Self.tag(machine: machine, pane: pane)))
        }
        seen[machine] = current
        return events
    }

    /// Forget everything, for a disconnect: the next pairing re-seeds.
    mutating func reset() {
        seen = [:]
        seeded = []
    }

    /// APNs caps a collapse id at 64 bytes, and the host truncates the same way.
    private static func tag(machine: String, pane: String) -> String {
        String("pilus-blocked-\(machine)-\(pane)".prefix(64))
    }

    /// How an agent kind reads in an alert; mirrors `kind_label` on the host.
    static func label(_ kind: String) -> String {
        switch kind {
        case "": return "An agent"
        case "opencode": return "OpenCode"
        default: return kind.prefix(1).uppercased() + kind.dropFirst()
        }
    }
}
