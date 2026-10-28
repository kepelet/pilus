import SwiftUI

// The daemon's `git.status` and `git.log` replies, as the hub relays them.
// Decoding is lenient on purpose, as in DiffModels: a field an older daemon
// omits reads as empty, so a newer or older Luvus still renders a list.

struct GitStatusReply: Decodable {
    let branch: String?
    let upstream: String?
    let ahead: Int?
    let behind: Int?
}

struct GitCommitRow: Decodable, Identifiable, Hashable {
    let sha: String
    let subject: String?
    let author: String?
    /// The daemon's own wording (`4 hours ago`).
    let when: String?
    /// Git's decoration, such as `(HEAD -> main, origin/main, tag: v1)`.
    let refs: String?

    var id: String { sha }
    var decorations: [GitDecoration] { GitDecoration.parse(refs ?? "") }
}

struct GitLogReply: Decodable {
    let commits: [GitCommitRow]
}

/// A branch or tag a commit carries.
struct GitDecoration: Hashable {
    enum Kind { case head, branch, tag }

    let name: String
    let kind: Kind

    static func parse(_ decoration: String) -> [GitDecoration] {
        decoration
            .trimmingCharacters(in: .whitespaces)
            .trimmingCharacters(in: CharacterSet(charactersIn: "()"))
            .split(separator: ",")
            .map { $0.trimmingCharacters(in: .whitespaces) }
            .filter { !$0.isEmpty }
            .map { part in
                if part.hasPrefix("HEAD -> ") {
                    GitDecoration(name: String(part.dropFirst("HEAD -> ".count)), kind: .head)
                } else if part.hasPrefix("tag: ") {
                    GitDecoration(name: String(part.dropFirst("tag: ".count)), kind: .tag)
                } else if part == "HEAD" {
                    GitDecoration(name: part, kind: .head)
                } else {
                    GitDecoration(name: part, kind: .branch)
                }
            }
    }
}

/// The history of an agent's workspace, with its uncommitted changes on top.
/// The daemon answers for the workspace it has active, so the agent's is
/// focused first. Read only: the phone never writes to a checkout.
@MainActor
@Observable
final class GitLogModel {
    /// Commits the first read asks for, and each "Show more" adds.
    static let page = 50

    let machine: String
    let agent: Agent?
    /// The working tree's changed files, when this device may read a diff.
    let changes: DiffReviewModel?

    var status: GitStatusReply?
    var commits: [GitCommitRow] = []
    var loaded = false
    var error: String?
    private var limit = GitLogModel.page
    private var focused = false

    private let store: AppStore

    init(store: AppStore, machine: String, agent: Agent? = nil) {
        self.store = store
        self.machine = machine
        self.agent = agent
        changes = store.canViewDiff ? DiffReviewModel(store: store, machine: machine, agent: agent) : nil
    }

    /// A full page means older commits may exist.
    var mayHaveMore: Bool { commits.count >= limit }

    /// The checked-out branch: the daemon's word, else the agent's, else the
    /// one the newest commit's HEAD points at.
    var branch: String? {
        let head = commits.first?.decorations.first { $0.kind == .head && $0.name != "HEAD" }?.name
        return [status?.branch, agent?.branch, head].compactMap { $0 }.first { !$0.isEmpty }
    }

    func load() async {
        error = nil
        defer { loaded = true }
        if !focused, let id = agent?.workspace_id {
            focused = true
            let _: EmptyReply? = try? await store.call(
                method: "workspace.focus", params: ["workspace_id": id], machine: machine)
        }
        await changes?.load(rescan: true)
        do {
            // The daemon reads the count from `n`; `limit` is ignored.
            let log: GitLogReply = try await store.call(
                method: "git.log", params: ["n": limit], machine: machine)
            commits = log.commits
            // The branch line is context; the history is worth showing
            // without it.
            status = try? await store.call(method: "git.status", machine: machine)
        } catch {
            self.error = error.localizedDescription
        }
    }

    func showMore() async {
        limit += Self.page
        await load()
    }
}

// MARK: - Entry

/// The log as a sheet from an agent's screen.
struct GitLogSheet: View {
    let agent: Agent
    @Environment(AppStore.self) private var store

    var body: some View {
        NavigationStack {
            GitLogView(model: GitLogModel(store: store, machine: agent.machine, agent: agent))
        }
    }
}

// MARK: - Commits

struct GitLogView: View {
    @State var model: GitLogModel
    @State private var loadingMore = false

    var body: some View {
        List {
            if let error = model.error {
                Section {
                    Text(error).font(.footnote).foregroundStyle(.red)
                }
            }
            if let branch = model.branch {
                Section { branchRow(model.status, branch: branch) }
            }
            if let changes = model.changes, !changes.files.isEmpty {
                Section("Changes") {
                    ForEach(changes.files) { file in
                        NavigationLink {
                            DiffFileView(model: changes, file: file)
                        } label: {
                            DiffFileLabel(file: file)
                                .alignmentGuide(.listRowSeparatorLeading) { _ in 0 }
                        }
                    }
                }
            }
            Section {
                ForEach(model.commits) { commit in
                    commitRow(commit)
                }
                if model.mayHaveMore {
                    Button {
                        loadingMore = true
                        Task {
                            await model.showMore()
                            loadingMore = false
                        }
                    } label: {
                        HStack {
                            Spacer()
                            if loadingMore { ProgressView() } else { Text("Show more") }
                            Spacer()
                        }
                    }
                    .disabled(loadingMore)
                }
            } header: {
                if model.changes?.files.isEmpty == false { Text("Commits") }
            } footer: {
                Text(model.agent?.workspace_id == nil
                     ? "History of the workspace open on the host."
                     : "History of this agent's workspace.")
            }
        }
        .navigationTitle("History")
        .navigationBarTitleDisplayMode(.inline)
        .overlay {
            if !model.loaded {
                ProgressView()
            } else if model.commits.isEmpty && model.error == nil {
                ContentUnavailableView("No commits", systemImage: "clock.arrow.circlepath")
            }
        }
        .refreshable { await model.load() }
        .task { await model.load() }
    }

    private func branchRow(_ status: GitStatusReply?, branch: String) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 6) {
                Text(branch).font(.body.weight(.semibold))
                Spacer()
                if let ahead = status?.ahead, ahead > 0 {
                    Text("↑\(ahead)").font(.footnote.monospacedDigit())
                }
                if let behind = status?.behind, behind > 0 {
                    Text("↓\(behind)").font(.footnote.monospacedDigit())
                }
            }
            if let upstream = status?.upstream, !upstream.isEmpty {
                Text("tracks \(upstream)")
                    .font(.footnote)
                    .foregroundStyle(.secondary)
            }
        }
    }

    private func commitRow(_ commit: GitCommitRow) -> some View {
        VStack(alignment: .leading, spacing: 5) {
            Text(commit.subject ?? "")
                .font(.body)
                .lineLimit(3)
            let decorations = commit.decorations
            if !decorations.isEmpty {
                // Wraps rather than scrolls: a commit with many refs stays
                // readable without a nested scroll view in a list row.
                FlowRefs(decorations: decorations)
            }
            HStack(spacing: 6) {
                Text(commit.sha)
                    .font(.system(.caption, design: .monospaced))
                if let author = commit.author, !author.isEmpty {
                    Text("· \(author)").lineLimit(1)
                }
                if let when = commit.when, !when.isEmpty {
                    Text("· \(when)").lineLimit(1)
                }
            }
            .font(.caption)
            .foregroundStyle(.secondary)
        }
        .padding(.vertical, 2)
    }
}

/// Branch and tag labels for one commit, wrapped onto as many lines as they
/// need.
private struct FlowRefs: View {
    let decorations: [GitDecoration]

    var body: some View {
        WrappingLayout(spacing: 4) {
            ForEach(decorations, id: \.self) { decoration in
                Label {
                    Text(decoration.name).lineLimit(1)
                } icon: {
                    Image(systemName: symbol(decoration.kind))
                }
                .font(.caption2.weight(.medium))
                .foregroundStyle(color(decoration.kind))
                .padding(.horizontal, 7)
                .padding(.vertical, 3)
                .background(color(decoration.kind).opacity(0.14), in: .capsule)
            }
        }
    }

    private func symbol(_ kind: GitDecoration.Kind) -> String {
        switch kind {
        case .head, .branch: "arrow.triangle.branch"
        case .tag: "tag"
        }
    }

    private func color(_ kind: GitDecoration.Kind) -> Color {
        switch kind {
        case .head: .blue
        case .branch: .secondary
        case .tag: .orange
        }
    }
}

/// Lays children left to right and wraps to a new line when one would
/// overflow the proposed width.
private struct WrappingLayout: Layout {
    var spacing: CGFloat

    func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) -> CGSize {
        arrange(in: proposal.width ?? .infinity, subviews: subviews).size
    }

    func placeSubviews(in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) {
        let placed = arrange(in: bounds.width, subviews: subviews)
        for (subview, origin) in zip(subviews, placed.origins) {
            subview.place(at: CGPoint(x: bounds.minX + origin.x, y: bounds.minY + origin.y), proposal: .unspecified)
        }
    }

    private func arrange(in width: CGFloat, subviews: Subviews) -> (size: CGSize, origins: [CGPoint]) {
        var origins: [CGPoint] = []
        var x: CGFloat = 0, y: CGFloat = 0, lineHeight: CGFloat = 0, widest: CGFloat = 0
        for subview in subviews {
            let size = subview.sizeThatFits(.unspecified)
            if x > 0, x + size.width > width {
                x = 0
                y += lineHeight + spacing
                lineHeight = 0
            }
            origins.append(CGPoint(x: x, y: y))
            x += size.width + spacing
            lineHeight = max(lineHeight, size.height)
            widest = max(widest, x - spacing)
        }
        return (CGSize(width: widest, height: y + lineHeight), origins)
    }
}
