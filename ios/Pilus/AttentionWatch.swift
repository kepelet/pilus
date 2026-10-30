import Foundation

/// Spots the moment an agent starts waiting on the user, or finishes.
///
/// Each refresh hands over one machine's `agent.list`; the watch folds it
/// into transitions: a pane that was not `blocked` and now is, one that
/// stops being `blocked`, or one that goes from `working` to `done`. It touches no UI, so the rules stay testable and the
/// store decides how to surface them. Mirrors `gui/attention.rs` on desktop.
struct AttentionWatch {
    enum Event: Equatable {
        case blocked(Blocked)
        /// The agent finished the turn it was running.
        case finished(Finished)
        /// The agent no longer waits or reports done, so its notification is
        /// stale.
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

    struct Finished: Equatable {
        /// Stable per pane, and apart from the blocked tag so one alert never
        /// replaces the other. Matches `tag_for` in `push.rs`.
        let tag: String
        let machine: String
        let agent: String
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
            let tag = Self.tag("blocked", machine: machine, pane: agent.pane)
            let doneTag = Self.tag("done", machine: machine, pane: agent.pane)
            let was = previous[agent.pane]
            let wasBlocked = was == "blocked"
            let isBlocked = agent.status == "blocked"
            let session = [agent.agent_session_title, agent.name, agent.workspace_name]
                .compactMap { $0 }
                .first { !$0.isEmpty } ?? ""
            if isSeeded && isBlocked && !wasBlocked {
                events.append(.blocked(Blocked(
                    tag: tag,
                    machine: machine,
                    agent: Self.label(agent.kind),
                    session: session
                )))
            } else if wasBlocked && !isBlocked {
                events.append(.cleared(tag: tag))
            }
            // Only a turn that was running counts as finishing: an agent that
            // first shows up already `done` is history, not news.
            if isSeeded && agent.status == "done" && was == "working" {
                events.append(.finished(Finished(
                    tag: doneTag,
                    machine: machine,
                    agent: Self.label(agent.kind),
                    session: session
                )))
            } else if was == "done" && agent.status != "done" {
                events.append(.cleared(tag: doneTag))
            }
            current[agent.pane] = agent.status ?? ""
        }
        // A pane that vanished while blocked (the agent exited) leaves its
        // banner behind unless it is cleared too.
        for (pane, status) in previous where current[pane] == nil {
            if status == "blocked" {
                events.append(.cleared(tag: Self.tag("blocked", machine: machine, pane: pane)))
            } else if status == "done" {
                events.append(.cleared(tag: Self.tag("done", machine: machine, pane: pane)))
            }
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
    private static func tag(_ kind: String, machine: String, pane: String) -> String {
        String("pilus-\(kind)-\(machine)-\(pane)".prefix(64))
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
