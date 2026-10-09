import SwiftUI

/// Stand-in notification center behind the bell: hardcoded agent-status
/// items rendered like the home list (plain rows, monochrome icons) until
/// the daemon grows a real notifications feed to bind to.
struct NotificationsView: View {
    /// One row: an optional task-status asset, an optional agent-kind asset,
    /// headline, detail line, and a relative timestamp.
    private struct Item: Identifiable {
        let id = UUID()
        var taskStatus: String?
        var agentKind: String?
        let title: String
        let detail: String
        let time: String
    }

    private let items: [Item] = [
        Item(taskStatus: "done",
             title: "Task 20 is done",
             detail: "Claude Code finished “test from mobile” in pilus",
             time: "2m"),
        Item(agentKind: "devin",
             title: "Devin needs your input",
             detail: "Waiting for permission to run `git push` in pilus",
             time: "14m"),
        Item(taskStatus: "failed",
             title: "Automation 20 failed",
             detail: "build-and-test exited with code 1 in pilus",
             time: "1h"),
    ]

    var body: some View {
        ScrollView {
            LazyVStack(alignment: .leading, spacing: 0) {
                ForEach(items) { item in
                    row(item)
                }
            }
            .padding(.horizontal, 20)
            .padding(.top, 12)
        }
        .navigationTitle("Notifications")
        .navigationBarTitleDisplayMode(.inline)
    }

    private func row(_ item: Item) -> some View {
        HStack(alignment: .top, spacing: 12) {
            if let status = item.taskStatus {
                TaskIconView(status: status, size: 26)
            } else {
                AgentIconView(kind: item.agentKind ?? "", size: 24)
            }
            VStack(alignment: .leading, spacing: 2) {
                Text(item.title)
                    .font(.body.weight(.medium))
                Text(item.detail)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(2)
            }
            Spacer(minLength: 8)
            Text(item.time)
                .font(.caption)
                .foregroundStyle(.tertiary)
        }
        .padding(.vertical, 10)
    }
}
