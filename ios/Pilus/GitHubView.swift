import SwiftUI

// GitHub issues and pull requests for a workspace, through the connector's
// `hub.gh.*` methods. The connector runs the GitHub CLI on the machine that
// owns the checkout, so the phone needs no GitHub login of its own and only
// ever sees what that machine's account can. Reads need a Control device;
// comments and pull requests need Operate, and the screens show only what the
// device's level carries.

// MARK: - Replies

struct GHLabel: Decodable, Hashable {
    let name: String
    let color: String?
}

struct GHRepo: Decodable {
    let name: String
    let url: String
    let default_branch: String?
    let can_write: Bool?
    let issues_enabled: Bool?
}

struct GHStatus: Decodable {
    /// `ready`, `missing`, `signed_out`, `no_repo`, or `failed`.
    let state: String
    let repo: GHRepo?
    /// What to do about a state that is not `ready`, in a sentence.
    let advice: String?
}

struct GHIssue: Decodable, Identifiable, Hashable {
    let number: Int
    let title: String
    let state: String
    let author: String
    let labels: [GHLabel]
    let updated_at: String
    let url: String
    let body: String

    var id: Int { number }
    var isOpen: Bool { state == "OPEN" }
}

struct GHIssueList: Decodable {
    let issues: [GHIssue]
}

struct GHComment: Decodable {
    let author: String
    let body: String
    let created_at: String
    let url: String
}

struct GHIssueDetail: Decodable {
    let issue: GHIssue
    let assignees: [String]
    let created_at: String
    let comments: [GHComment]
}

struct GHPull: Decodable, Identifiable, Hashable {
    let number: Int
    let title: String
    /// `OPEN`, `CLOSED`, or `MERGED`.
    let state: String
    let draft: Bool
    let author: String
    let head: String
    let base: String
    /// `APPROVED`, `CHANGES_REQUESTED`, `REVIEW_REQUIRED`, or empty.
    let review: String
    let labels: [GHLabel]
    let updated_at: String
    let url: String

    var id: Int { number }

    var reviewText: String? {
        switch review {
        case "APPROVED": "approved"
        case "CHANGES_REQUESTED": "changes requested"
        case "REVIEW_REQUIRED": "review required"
        default: nil
        }
    }
}

struct GHPullList: Decodable {
    let pulls: [GHPull]
}

struct GHChecks: Decodable {
    let passed: Int
    let failed: Int
    let pending: Int
}

struct GHPullDetail: Decodable {
    let pull: GHPull
    let body: String
    let comments: [GHComment]
    let additions: Int
    let deletions: Int
    let changed_files: Int
    let mergeable: String
    let checks: GHChecks
}

struct GHPullDraft: Decodable {
    let branch: String
    let base: String
    let title: String
    let commits: [String]
    let needs_push: Bool
    let existing: GHPull?
}

struct GHCreated: Decodable {
    let url: String
    let number: Int?
    let pushed: Bool
}

struct GHUrl: Decodable {
    let url: String
}

// MARK: - Attaching an issue to a prompt

extension GHIssue {
    /// Most of an issue's body handed to an agent, matching the desktop.
    private static let contextLimit = 8_000

    /// The issue as context for an agent's prompt. Worded exactly as the
    /// desktop's `Issue::as_context`, so both surfaces hand an agent the
    /// same text.
    var context: String {
        var text = "GitHub issue #\(number): \(title)"
        if !url.isEmpty { text += "\n\(url)" }
        let trimmed = body.trimmingCharacters(in: .whitespacesAndNewlines)
        if !trimmed.isEmpty {
            text += "\n\n"
            if trimmed.count > Self.contextLimit {
                text += String(trimmed.prefix(Self.contextLimit)) + "\n… (issue body truncated)"
            } else {
                text += trimmed
            }
        }
        return text
    }

    /// `prompt` with this issue attached beneath it. An empty prompt gets a
    /// default, since the issue is the work.
    func attach(to prompt: String) -> String {
        var lead = prompt.trimmingCharacters(in: .whitespacesAndNewlines)
        if lead.isEmpty { lead = "Work on this GitHub issue." }
        return "\(lead)\n\n---\n\(context)"
    }
}

// MARK: - Model

/// One workspace's GitHub, read through the hub.
@MainActor
@Observable
final class GitHubModel {
    enum Section: String, CaseIterable, Identifiable {
        case issues = "Issues"
        case pulls = "Pull requests"
        var id: String { rawValue }
    }

    let machine: String
    let path: String

    var status: GHStatus?
    var issues: [GHIssue]?
    var pulls: [GHPull]?
    var section = Section.issues
    /// `open`, `closed`, or `merged` (pull requests only).
    var filter = "open"
    var error: String?
    var loading = false

    private let store: AppStore

    init(store: AppStore, machine: String, path: String) {
        self.store = store
        self.machine = machine
        self.path = path
    }

    var canComment: Bool { store.supports("hub.gh.issue_comment", on: machine) }
    var canSubmit: Bool { store.supports("hub.gh.pull_create", on: machine) }

    func call<Reply: Decodable>(_ method: String, _ extra: [String: Any] = [:]) async throws -> Reply {
        var params = extra
        params["path"] = path
        return try await store.call(method: method, params: params, machine: machine)
    }

    /// Read the list for the open section, checking `gh` first when it is not
    /// yet known to work.
    func load() async {
        loading = true
        error = nil
        defer { loading = false }
        do {
            if status?.state != "ready" {
                status = try await call("hub.gh.status")
            }
            guard status?.state == "ready" else { return }
            switch section {
            case .issues:
                let list: GHIssueList = try await call("hub.gh.issues", ["state": filter])
                issues = list.issues
            case .pulls:
                let list: GHPullList = try await call("hub.gh.pulls", ["state": filter])
                pulls = list.pulls
            }
        } catch {
            self.error = error.localizedDescription
        }
    }

    func setSection(_ next: Section) async {
        guard next != section else { return }
        section = next
        if next == .issues && filter == "merged" { filter = "open" }
        await load()
    }

    func setFilter(_ next: String) async {
        guard next != filter else { return }
        filter = next
        if section == .issues { issues = nil } else { pulls = nil }
        await load()
    }

    func issue(_ number: Int) async throws -> GHIssueDetail {
        try await call("hub.gh.issue", ["number": number])
    }

    func pull(_ number: Int) async throws -> GHPullDetail {
        try await call("hub.gh.pull", ["number": number])
    }

    func commentOnIssue(_ number: Int, body: String) async throws {
        let _: GHUrl = try await call("hub.gh.issue_comment", ["number": number, "body": body])
    }

    func commentOnPull(_ number: Int, body: String) async throws {
        let _: GHUrl = try await call("hub.gh.pull_comment", ["number": number, "body": body])
    }

    func draft() async throws -> GHPullDraft {
        try await call("hub.gh.pull_draft")
    }

    func createPull(title: String, body: String, base: String, draft: Bool) async throws -> GHCreated {
        try await call(
            "hub.gh.pull_create",
            ["title": title, "body": body, "base": base, "draft": draft]
        )
    }
}

// MARK: - Entry points

/// GitHub as a sheet from an agent's screen.
struct GitHubSheet: View {
    let agent: Agent
    @Environment(AppStore.self) private var store

    var body: some View {
        NavigationStack {
            GitHubView(model: GitHubModel(store: store, machine: agent.machine, path: agent.cwd ?? ""))
        }
    }
}

// MARK: - Lists

struct GitHubView: View {
    @State var model: GitHubModel
    @Environment(\.dismiss) private var dismiss
    @State private var submitting = false

    var body: some View {
        content
            .navigationTitle(model.status?.repo?.name ?? "GitHub")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Done") { dismiss() }
                }
                if model.status?.state == "ready" {
                    ToolbarItem(placement: .primaryAction) { filterMenu }
                }
            }
            .navigationDestination(for: GHIssue.self) { issue in
                IssueDetailView(model: model, issue: issue)
            }
            .navigationDestination(for: GHPull.self) { pull in
                PullDetailView(model: model, pull: pull)
            }
            .sheet(isPresented: $submitting) {
                NewPullView(model: model) { _ in
                    Task { await model.load() }
                }
            }
            .task { await model.load() }
    }

    @ViewBuilder
    private var content: some View {
        if let status = model.status, status.state != "ready" {
            ContentUnavailableView(
                "GitHub isn't set up here",
                systemImage: "exclamationmark.triangle",
                description: Text(status.advice ?? "GitHub CLI isn't working on this machine."),
                actions: {
                    Button("Check again") { Task { await model.load() } }
                }
            )
        } else if let error = model.error, currentCount == nil {
            ContentUnavailableView(
                "Couldn't load GitHub",
                systemImage: "exclamationmark.triangle",
                description: Text(error),
                actions: {
                    Button("Try again") { Task { await model.load() } }
                }
            )
        } else {
            list
        }
    }

    private var currentCount: Int? {
        model.section == .issues ? model.issues?.count : model.pulls?.count
    }

    private var list: some View {
        List {
            Section {
                Picker("Show", selection: Binding(
                    get: { model.section },
                    set: { next in Task { await model.setSection(next) } }
                )) {
                    ForEach(GitHubModel.Section.allCases) { section in
                        Text(section.rawValue).tag(section)
                    }
                }
                .pickerStyle(.segmented)
                .listRowBackground(Color.clear)
                .listRowInsets(EdgeInsets())
            }
            switch model.section {
            case .issues:
                if let issues = model.issues {
                    ForEach(issues) { issue in
                        NavigationLink(value: issue) { IssueRow(issue: issue) }
                    }
                    if issues.isEmpty { empty("No \(model.filter) issues.") }
                } else {
                    loadingRow
                }
            case .pulls:
                if let pulls = model.pulls {
                    ForEach(pulls) { pull in
                        NavigationLink(value: pull) { PullRow(pull: pull) }
                    }
                    if pulls.isEmpty { empty("No \(model.filter) pull requests.") }
                } else {
                    loadingRow
                }
            }
        }
        .listStyle(.plain)
        .refreshable { await model.load() }
        .safeAreaInset(edge: .bottom) {
            if model.section == .pulls, model.canSubmit {
                Button {
                    submitting = true
                } label: {
                    Label("Submit this branch", systemImage: "arrow.up.right.circle")
                        .frame(maxWidth: .infinity)
                }
                .buttonStyle(.borderedProminent)
                .padding()
            }
        }
    }

    private var loadingRow: some View {
        HStack {
            Spacer()
            ProgressView()
            Spacer()
        }
        .listRowSeparator(.hidden)
    }

    private func empty(_ text: String) -> some View {
        Text(text)
            .font(.footnote)
            .foregroundStyle(.secondary)
            .frame(maxWidth: .infinity, alignment: .center)
            .listRowSeparator(.hidden)
    }

    private var filterMenu: some View {
        Menu {
            ForEach(filters, id: \.self) { value in
                Button {
                    Task { await model.setFilter(value) }
                } label: {
                    if model.filter == value {
                        Label(value.capitalized, systemImage: "checkmark")
                    } else {
                        Text(value.capitalized)
                    }
                }
            }
        } label: {
            Image(systemName: "line.3.horizontal.decrease.circle")
        }
        .accessibilityLabel("Filter")
    }

    private var filters: [String] {
        model.section == .issues ? ["open", "closed"] : ["open", "merged", "closed"]
    }
}

private struct IssueRow: View {
    let issue: GHIssue

    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            Image(systemName: issue.isOpen ? "circle.circle" : "checkmark.circle")
                .foregroundStyle(issue.isOpen ? Color.green : Color.purple)
                .padding(.top, 2)
            VStack(alignment: .leading, spacing: 4) {
                Text(issue.title)
                    .font(.body.weight(.medium))
                    .multilineTextAlignment(.leading)
                Text("#\(issue.number) · \(issue.author)\(ghAge(issue.updated_at))")
                    .font(.footnote)
                    .foregroundStyle(.secondary)
                LabelChips(labels: issue.labels)
            }
        }
        .padding(.vertical, 2)
    }
}

private struct PullRow: View {
    let pull: GHPull

    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            Image(systemName: ghPullSymbol(pull))
                .foregroundStyle(ghPullTint(pull))
                .padding(.top, 2)
            VStack(alignment: .leading, spacing: 4) {
                Text(pull.title)
                    .font(.body.weight(.medium))
                    .multilineTextAlignment(.leading)
                Text("#\(pull.number) · \(pull.author) · \(pull.head) → \(pull.base)\(ghAge(pull.updated_at))")
                    .font(.footnote)
                    .foregroundStyle(.secondary)
                    .lineLimit(2)
                if let review = pull.reviewText {
                    Text(review).font(.caption).foregroundStyle(.secondary)
                }
                LabelChips(labels: pull.labels)
            }
        }
        .padding(.vertical, 2)
    }
}

private struct LabelChips: View {
    let labels: [GHLabel]

    var body: some View {
        if !labels.isEmpty {
            HStack(spacing: 4) {
                ForEach(labels.prefix(3), id: \.name) { label in
                    Text(label.name)
                        .font(.caption2)
                        .padding(.horizontal, 6)
                        .padding(.vertical, 2)
                        .overlay(Capsule().strokeBorder(.quaternary))
                }
            }
        }
    }
}

// MARK: - Details

/// Shared by both detail screens: the description, the comments, and the
/// comment bar when the device may write.
private struct ThreadView: View {
    let header: AnyView
    let description: String
    let comments: [GHComment]
    let canComment: Bool
    let reload: () async -> Void
    let send: (String) async throws -> Void

    @State private var draft = ""
    @State private var sending = false
    @State private var failure: String?

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 14) {
                header
                card(description.isEmpty ? "No description provided." : description, muted: description.isEmpty)
                ForEach(Array(comments.enumerated()), id: \.offset) { _, comment in
                    VStack(alignment: .leading, spacing: 6) {
                        Text("\(comment.author) commented\(ghAge(comment.created_at))")
                            .font(.footnote)
                            .foregroundStyle(.secondary)
                        Text(ghMarkdown(comment.body))
                            .textSelection(.enabled)
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(12)
                    .overlay(RoundedRectangle(cornerRadius: 10).strokeBorder(.quaternary))
                }
            }
            .padding()
        }
        .refreshable { await reload() }
        .safeAreaInset(edge: .bottom) {
            if canComment { commentBar }
        }
    }

    private func card(_ text: String, muted: Bool) -> some View {
        Text(ghMarkdown(text))
            .foregroundStyle(muted ? Color.secondary : Color.primary)
            .textSelection(.enabled)
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(12)
            .overlay(RoundedRectangle(cornerRadius: 10).strokeBorder(.quaternary))
    }

    private var commentBar: some View {
        VStack(spacing: 4) {
            if let failure {
                Text(failure).font(.footnote).foregroundStyle(.red)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            HStack(alignment: .bottom, spacing: 8) {
                TextField("Leave a comment", text: $draft, axis: .vertical)
                    .lineLimit(1...5)
                    .textFieldStyle(.roundedBorder)
                Button {
                    post()
                } label: {
                    if sending {
                        ProgressView().frame(width: 28, height: 28)
                    } else {
                        SendArrow(enabled: canPost, size: 28)
                    }
                }
                .disabled(!canPost || sending)
                .accessibilityLabel("Comment")
            }
        }
        .padding(.horizontal)
        .padding(.vertical, 8)
        .background(.bar)
    }

    private var canPost: Bool {
        !draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
    }

    private func post() {
        let text = draft
        sending = true
        failure = nil
        Task {
            do {
                try await send(text)
                draft = ""
                await reload()
            } catch {
                failure = error.localizedDescription
            }
            sending = false
        }
    }
}

struct IssueDetailView: View {
    let model: GitHubModel
    let issue: GHIssue
    @State private var detail: GHIssueDetail?
    @State private var error: String?

    var body: some View {
        Group {
            if let detail {
                ThreadView(
                    header: AnyView(header(detail)),
                    description: detail.issue.body,
                    comments: detail.comments,
                    canComment: model.canComment,
                    reload: { await load() },
                    send: { try await model.commentOnIssue(issue.number, body: $0) }
                )
            } else if let error {
                ContentUnavailableView("Couldn't load the issue", systemImage: "exclamationmark.triangle", description: Text(error))
            } else {
                ProgressView()
            }
        }
        .navigationTitle("#\(issue.number)")
        .navigationBarTitleDisplayMode(.inline)
        .toolbar { OpenOnGitHub(url: issue.url) }
        .task { await load() }
    }

    private func header(_ detail: GHIssueDetail) -> some View {
        var meta = "opened by \(detail.issue.author)\(ghAge(detail.created_at))"
        if !detail.assignees.isEmpty { meta += " · assigned to \(detail.assignees.joined(separator: ", "))" }
        return VStack(alignment: .leading, spacing: 6) {
            Text(detail.issue.title).font(.title3.weight(.bold))
            Text(meta).font(.footnote).foregroundStyle(.secondary)
            LabelChips(labels: detail.issue.labels)
        }
    }

    private func load() async {
        do {
            detail = try await model.issue(issue.number)
            error = nil
        } catch {
            if detail == nil { self.error = error.localizedDescription }
        }
    }
}

struct PullDetailView: View {
    let model: GitHubModel
    let pull: GHPull
    @State private var detail: GHPullDetail?
    @State private var error: String?

    var body: some View {
        Group {
            if let detail {
                ThreadView(
                    header: AnyView(header(detail)),
                    description: detail.body,
                    comments: detail.comments,
                    canComment: model.canComment,
                    reload: { await load() },
                    send: { try await model.commentOnPull(pull.number, body: $0) }
                )
            } else if let error {
                ContentUnavailableView("Couldn't load the pull request", systemImage: "exclamationmark.triangle", description: Text(error))
            } else {
                ProgressView()
            }
        }
        .navigationTitle("#\(pull.number)")
        .navigationBarTitleDisplayMode(.inline)
        .toolbar { OpenOnGitHub(url: pull.url) }
        .task { await load() }
    }

    private func header(_ detail: GHPullDetail) -> some View {
        let checks = detail.checks
        let failing = checks.failed > 0 || detail.mergeable == "CONFLICTING"
        var facts = "+\(detail.additions) −\(detail.deletions) in \(detail.changed_files) file\(detail.changed_files == 1 ? "" : "s")"
        if checks.passed + checks.failed + checks.pending > 0 {
            facts += " · checks: \(checks.passed) passed, \(checks.failed) failed, \(checks.pending) pending"
        }
        if detail.mergeable == "CONFLICTING" { facts += " · has conflicts" }
        return VStack(alignment: .leading, spacing: 6) {
            HStack(alignment: .top, spacing: 8) {
                Image(systemName: ghPullSymbol(detail.pull)).foregroundStyle(ghPullTint(detail.pull))
                Text(detail.pull.title).font(.title3.weight(.bold))
            }
            Text("\(detail.pull.author) wants to merge \(detail.pull.head) into \(detail.pull.base)")
                .font(.footnote).foregroundStyle(.secondary)
            if let review = detail.pull.reviewText {
                Text(review).font(.footnote).foregroundStyle(.secondary)
            }
            Text(facts).font(.footnote).foregroundStyle(failing ? Color.red : Color.secondary)
            LabelChips(labels: detail.pull.labels)
        }
    }

    private func load() async {
        do {
            detail = try await model.pull(pull.number)
            error = nil
        } catch {
            if detail == nil { self.error = error.localizedDescription }
        }
    }
}

private struct OpenOnGitHub: ToolbarContent {
    let url: String
    @Environment(\.openURL) private var openURL

    var body: some ToolbarContent {
        ToolbarItem(placement: .primaryAction) {
            Button {
                if let link = URL(string: url) { openURL(link) }
            } label: {
                Image(systemName: "arrow.up.right.square")
            }
            .accessibilityLabel("Open on GitHub")
        }
    }
}

// MARK: - Submitting a pull request

/// Open a pull request from the checkout's current branch. The host pushes
/// that one branch first when its remote lacks the commits.
struct NewPullView: View {
    let model: GitHubModel
    let created: (Int?) -> Void
    @Environment(\.dismiss) private var dismiss

    @State private var draft: GHPullDraft?
    @State private var title = ""
    @State private var detail = ""
    @State private var asDraft = false
    @State private var submitting = false
    @State private var error: String?

    var body: some View {
        NavigationStack {
            Group {
                if let draft {
                    if let existing = draft.existing {
                        ContentUnavailableView(
                            "Already open",
                            systemImage: "arrow.triangle.pull",
                            description: Text("\(draft.branch) already has pull request #\(existing.number): \(existing.title)")
                        )
                    } else {
                        form(draft)
                    }
                } else if let error {
                    ContentUnavailableView("Can't submit yet", systemImage: "exclamationmark.triangle", description: Text(error))
                } else {
                    ProgressView()
                }
            }
            .navigationTitle("Submit pull request")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { dismiss() }
                }
                ToolbarItem(placement: .confirmationAction) {
                    if submitting {
                        ProgressView()
                    } else if let draft, draft.existing == nil {
                        Button(draft.needs_push ? "Push and submit" : "Submit") { submit(draft) }
                            .buttonStyle(.borderedProminent)
                            .disabled(title.trimmingCharacters(in: .whitespaces).isEmpty)
                    }
                }
            }
            .task {
                do {
                    let read = try await model.draft()
                    title = read.title
                    draft = read
                } catch {
                    self.error = error.localizedDescription
                }
            }
        }
    }

    private func form(_ draft: GHPullDraft) -> some View {
        Form {
            if let error {
                Text(error).font(.footnote).foregroundStyle(.red)
            }
            Section("Branch") {
                Text("\(draft.branch) → \(draft.base)")
                    .font(.system(.footnote, design: .monospaced))
                ForEach(Array(draft.commits.prefix(6).enumerated()), id: \.offset) { _, subject in
                    Text(subject).font(.footnote).foregroundStyle(.secondary)
                }
                if draft.commits.count > 6 {
                    Text("and \(draft.commits.count - 6) more").font(.footnote).foregroundStyle(.secondary)
                }
            }
            Section("Title") {
                TextField("Title", text: $title)
            }
            Section("Description") {
                TextField("Describe the change (optional)", text: $detail, axis: .vertical)
                    .lineLimit(3...8)
            }
            Section {
                Toggle("Open as a draft", isOn: $asDraft)
            } footer: {
                if draft.needs_push {
                    Text("This pushes \(draft.branch) to its remote first. Nothing else is pushed.")
                }
            }
        }
    }

    private func submit(_ draft: GHPullDraft) {
        submitting = true
        error = nil
        Task {
            do {
                let made = try await model.createPull(
                    title: title, body: detail, base: draft.base, draft: asDraft)
                created(made.number)
                dismiss()
            } catch let failure as PilusError where failure.isUnconfirmed {
                // The connection dropped mid-request, so the pull request may
                // exist. Resending could not duplicate it (GitHub refuses a
                // second one for the branch), but the list is the truth.
                error = "The connection dropped. Check the pull requests list before trying again."
            } catch {
                self.error = error.localizedDescription
            }
            submitting = false
        }
    }
}

// MARK: - Choosing an issue for a prompt

/// The open issues of a workspace, to attach one to a prompt.
struct IssuePickerSheet: View {
    let machine: String
    let path: String
    @Binding var selection: GHIssue?
    @Environment(AppStore.self) private var store
    @Environment(\.dismiss) private var dismiss

    @State private var issues: [GHIssue]?
    @State private var error: String?

    var body: some View {
        NavigationStack {
            Group {
                if let error {
                    ContentUnavailableView("Couldn't load issues", systemImage: "exclamationmark.triangle", description: Text(error))
                } else if let issues {
                    List {
                        if selection != nil {
                            Button("Don't attach an issue", role: .destructive) {
                                selection = nil
                                dismiss()
                            }
                        }
                        ForEach(issues) { issue in
                            Button {
                                selection = issue
                                dismiss()
                            } label: {
                                HStack {
                                    IssueRow(issue: issue)
                                    Spacer()
                                    if selection?.number == issue.number {
                                        Image(systemName: "checkmark").foregroundStyle(.tint)
                                    }
                                }
                            }
                            .tint(.primary)
                        }
                        if issues.isEmpty {
                            Text("No open issues.").font(.footnote).foregroundStyle(.secondary)
                        }
                    }
                    .listStyle(.plain)
                } else {
                    ProgressView()
                }
            }
            .navigationTitle("Attach an issue")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { dismiss() }
                }
            }
            .task { await load() }
        }
    }

    private func load() async {
        do {
            let list: GHIssueList = try await store.call(
                method: "hub.gh.issues", params: ["path": path, "state": "open"], machine: machine)
            issues = list.issues
        } catch {
            self.error = error.localizedDescription
        }
    }
}

// MARK: - Shared helpers

/// Markdown as styled text, keeping line breaks. Falls back to the raw text
/// when it does not parse.
private func ghMarkdown(_ source: String) -> AttributedString {
    let options = AttributedString.MarkdownParsingOptions(
        interpretedSyntax: .inlineOnlyPreservingWhitespace)
    return (try? AttributedString(markdown: source, options: options)) ?? AttributedString(source)
}

private func ghPullSymbol(_ pull: GHPull) -> String {
    switch (pull.state, pull.draft) {
    case ("MERGED", _): "arrow.triangle.merge"
    case ("CLOSED", _): "xmark.circle"
    case (_, true): "arrow.triangle.pull"
    default: "arrow.triangle.pull"
    }
}

private func ghPullTint(_ pull: GHPull) -> Color {
    switch (pull.state, pull.draft) {
    case ("MERGED", _): .purple
    case ("CLOSED", _): .red
    case (_, true): .secondary
    default: .green
    }
}

/// ` · 3d` for a GitHub timestamp, or nothing when it does not parse.
private func ghAge(_ timestamp: String) -> String {
    guard let date = ISO8601DateFormatter().date(from: timestamp) else { return "" }
    let seconds = max(0, Int(Date().timeIntervalSince(date)))
    let text: String
    switch seconds {
    case ..<60: text = "now"
    case ..<3600: text = "\(seconds / 60)m"
    case ..<86_400: text = "\(seconds / 3600)h"
    case ..<604_800: text = "\(seconds / 86_400)d"
    default: text = "\(seconds / 604_800)w"
    }
    return " · \(text)"
}
