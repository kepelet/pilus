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
        let tag: String
        let agent: String
        let workspace: String
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
                    agent: agent.title,
                    workspace: agent.workspace_name ?? ""
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

    private static func tag(machine: String, pane: String) -> String {
        "pilus-blocked-\(machine)-\(pane)"
    }
}
