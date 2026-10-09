import SwiftUI

/// What a send covers.
enum NoteSendScope: String, CaseIterable, Identifiable {
    case note, file, all
    var id: String { rawValue }
    var title: String {
        switch self {
        case .note: "This note"
        case .file: "This file"
        case .all: "All files"
        }
    }
}

/// The review on one machine: the daemon's changed files, the open file's
/// patch, and the notes. Everything is read through the hub, so a view-level
/// device sees the diff and a control-level one can also write notes.
@MainActor
@Observable
final class DiffReviewModel {
    let machine: String
    /// The agent the review was opened from; the default target for a send.
    let agent: Agent?

    var repo = ""
    var branch = ""
    var files: [DiffFileRow] = []
    var notes: [DiffNote] = []
    var loaded = false
    var busy = false
    var error: String?
    /// The last write's outcome, shown until the next action.
    var status: String?

    private let store: AppStore

    init(store: AppStore, machine: String, agent: Agent?) {
        self.store = store
        self.machine = machine
        self.agent = agent
    }

    var canWrite: Bool { store.canReviewDiff }

    var openNotes: [DiffNote] { notes.filter(\.isOpen) }

    func notes(for file: DiffFileRow) -> [DiffNote] {
        notes.filter { $0.path == file.path && $0.layer == file.layer }
    }

    /// The review follows whichever workspace the daemon has active, so a
    /// mismatch with the agent's own folder is worth saying out loud.
    var showsOtherWorkspace: Bool {
        guard let cwd = agent?.cwd, !repo.isEmpty else { return false }
        return !cwd.hasPrefix(repo)
    }

    func load(rescan: Bool = false) async {
        busy = true
        error = nil
        defer {
            busy = false
            loaded = true
        }
        do {
            if rescan {
                // A refresh the daemon refuses (a view-level device) only means
                // the list may be a scan old; it is not worth failing over.
                let _: EmptyReply? = try? await store.call(
                    method: "diff.refresh", machine: machine)
            }
            let list: DiffListReply = try await store.call(method: "diff.list", machine: machine)
            repo = list.repo ?? ""
            branch = list.branch ?? ""
            files = list.files
            let noted: DiffNotesReply = try await store.call(method: "diff.note.list", machine: machine)
            notes = noted.notes
        } catch {
            self.error = error.localizedDescription
        }
    }

    func patch(for file: DiffFileRow) async throws -> DiffFileReply {
        try await store.call(
            method: "diff.get",
            params: ["path": file.path, "layer": file.layer, "include_patch": true],
            machine: machine
        )
    }

    // MARK: Notes

    func addNote(file: DiffFileRow, range: NoteRange, kind: NoteKind, body: String) async -> Bool {
        var params: [String: Any] = [
            "file": file.path,
            "layer": file.layer,
            "kind": kind.rawValue,
            "body": body,
        ]
        params[range.side == .old ? "old_line" : "new_line"] = range.start
        if range.end != range.start { params["end_line"] = range.end }
        return await write("Note added", method: "diff.note.add", params: params)
    }

    func editNote(_ note: DiffNote, body: String) async -> Bool {
        await write("Note saved", method: "diff.note.edit", params: ["id": note.id, "body": body])
    }

    func setResolved(_ note: DiffNote, _ resolved: Bool) async {
        _ = await write(
            resolved ? "Note resolved" : "Note reopened",
            method: resolved ? "diff.note.resolve" : "diff.note.reopen",
            params: ["id": note.id]
        )
    }

    func delete(_ note: DiffNote) async {
        _ = await write("Note deleted", method: "diff.note.remove", params: ["id": note.id])
    }

    /// Hand notes to a live agent as one grouped message. Sending never
    /// resolves a note.
    func send(scope: NoteSendScope, note: DiffNote?, file: DiffFileRow?, to target: Agent) async -> Bool {
        var params: [String: Any] = ["to": target.pane]
        switch scope {
        case .note:
            guard let note else { return false }
            params["ids"] = [note.id]
        case .file:
            guard let file else { return false }
            let ids = notes(for: file).filter(\.isOpen).map(\.id)
            guard !ids.isEmpty else {
                status = "No open notes in this file"
                return false
            }
            params["ids"] = ids
        case .all:
            guard !openNotes.isEmpty else {
                status = "No open notes to send"
                return false
            }
            params["all_open"] = true
        }
        return await write("Sent to \(target.title)", method: "diff.note.send", params: params)
    }

    /// Point the daemon at the agent's workspace so the review shows its
    /// changes. The Mac's own window follows, which is why it is a button and
    /// not something the review does on its own.
    func showAgentWorkspace() async {
        guard let id = agent?.workspace_id else { return }
        busy = true
        defer { busy = false }
        do {
            let _: EmptyReply = try await store.call(
                method: "workspace.focus",
                params: ["workspace_id": id],
                machine: machine
            )
            await load(rescan: true)
        } catch {
            self.error = error.localizedDescription
        }
    }

    /// Run a note write, then reload so the screen shows the daemon's truth.
    private func write(_ done: String, method: String, params: [String: Any]) async -> Bool {
        status = nil
        do {
            let _: EmptyReply = try await store.call(method: method, params: params, machine: machine)
            status = done
            await load()
            return true
        } catch {
            status = error.localizedDescription
            return false
        }
    }
}

// MARK: - Entry

/// The review as a sheet from an agent's screen.
struct DiffReviewSheet: View {
    let agent: Agent
    @Environment(AppStore.self) private var store
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        NavigationStack {
            DiffReviewView(model: DiffReviewModel(store: store, machine: agent.machine, agent: agent))
                .toolbar {
                    ToolbarItem(placement: .topBarLeading) {
                        Button("Done") { dismiss() }
                    }
                }
        }
    }
}

// MARK: - Changed files

struct DiffReviewView: View {
    @State var model: DiffReviewModel
    @State private var sending = false

    var body: some View {
        List {
            if model.showsOtherWorkspace {
                Section {
                    VStack(alignment: .leading, spacing: 8) {
                        Text("These changes belong to the workspace open on the Mac, not this agent's folder.")
                            .font(.footnote)
                            .foregroundStyle(.secondary)
                        if model.agent?.workspace_id != nil {
                            Button("Show this agent's workspace") {
                                Task { await model.showAgentWorkspace() }
                            }
                            .font(.footnote.weight(.semibold))
                        }
                    }
                }
            }
            if let error = model.error {
                Section { Text(error).font(.footnote).foregroundStyle(.red) }
            }
            if model.loaded && model.files.isEmpty && model.error == nil {
                Section {
                    Text("No changes. Staged, working-tree and untracked changes appear here.")
                        .foregroundStyle(.secondary)
                }
            }
            ForEach(DiffLayer.allCases, id: \.self) { layer in
                let rows = model.files.filter { $0.layerKind == layer }
                if !rows.isEmpty {
                    Section(layer.title) {
                        ForEach(rows) { file in
                            NavigationLink {
                                DiffFileView(model: model, file: file)
                            } label: {
                                DiffFileLabel(file: file)
                            }
                        }
                    }
                }
            }
            if let status = model.status {
                Section { Text(status).font(.footnote).foregroundStyle(.secondary) }
            }
        }
        .navigationTitle(model.branch.isEmpty ? "Changes" : model.branch)
        .navigationBarTitleDisplayMode(.inline)
        .overlay {
            if !model.loaded { ProgressView() }
        }
        .toolbar {
            ToolbarItem(placement: .topBarTrailing) {
                if model.canWrite && !model.openNotes.isEmpty {
                    Button {
                        sending = true
                    } label: {
                        Label("Send \(model.openNotes.count)", systemImage: "paperplane")
                    }
                }
            }
            ToolbarItem(placement: .topBarTrailing) {
                Button {
                    Task { await model.load(rescan: true) }
                } label: {
                    Image(systemName: "arrow.clockwise")
                }
                .disabled(model.busy)
                .accessibilityLabel("Refresh")
            }
        }
        .sheet(isPresented: $sending) {
            SendNotesSheet(model: model, scope: .all, note: nil, file: nil)
        }
        .refreshable { await model.load(rescan: true) }
        .task {
            await model.load(rescan: true)
            // The Mac's files change while an agent works; stay current.
            while !Task.isCancelled {
                try? await Task.sleep(for: .seconds(5))
                await model.load()
            }
        }
    }
}

private struct DiffFileLabel: View {
    let file: DiffFileRow

    var body: some View {
        HStack(spacing: 10) {
            Text(file.statusGlyph)
                .font(.system(.footnote, design: .monospaced).weight(.bold))
                .foregroundStyle(tint)
                .frame(width: 14)
            VStack(alignment: .leading, spacing: 1) {
                Text(file.name)
                    .font(.body)
                    .lineLimit(1)
                if !file.directory.isEmpty {
                    Text(file.directory)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                        .truncationMode(.head)
                }
            }
            Spacer(minLength: 4)
            if let notes = file.notes, notes > 0 {
                Label("\(notes)", systemImage: "text.bubble")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            VStack(alignment: .trailing, spacing: 1) {
                if let additions = file.additions, additions > 0 {
                    Text("+\(additions)").foregroundStyle(.green)
                }
                if let deletions = file.deletions, deletions > 0 {
                    Text("−\(deletions)").foregroundStyle(.red)
                }
            }
            .font(.system(.caption2, design: .monospaced))
        }
    }

    private var tint: Color {
        switch (file.layerKind, file.status ?? "") {
        case (.untracked, _): .green
        case (.conflict, _), (_, "D"): .red
        case (_, "A"): .green
        default: .purple
        }
    }
}

// MARK: - One file

struct DiffFileView: View {
    let model: DiffReviewModel
    let file: DiffFileRow

    @State private var rows: [DiffRow] = []
    @State private var truncated = false
    @State private var binary = false
    @State private var loading = true
    @State private var loadError: String?
    @State private var selection: LineSelection?
    @State private var composing: NoteDraft?
    @State private var sending: DiffNote?
    @State private var confirmDelete: DiffNote?

    var body: some View {
        let placed = placeNotes(model.notes(for: file), in: rows)
        ScrollView {
            LazyVStack(alignment: .leading, spacing: 0) {
                if let loadError {
                    Text(loadError).font(.footnote).foregroundStyle(.red).padding()
                }
                if truncated {
                    Text("TRUNCATED: the daemon bounded this patch.")
                        .font(.caption)
                        .foregroundStyle(.red)
                        .padding(.horizontal)
                        .padding(.vertical, 6)
                }
                if !loading && rows.isEmpty && loadError == nil {
                    Text(binary ? "Binary file: no text diff" : "No textual changes")
                        .foregroundStyle(.secondary)
                        .padding()
                }
                ForEach(rows) { row in
                    DiffLineRow(row: row, selected: isSelected(row))
                        .contentShape(Rectangle())
                        .onTapGesture {
                            guard model.canWrite, row.kind != .hunk else { return }
                            selection = selecting(row.id, from: selection)
                        }
                    ForEach(placed.byRow[row.id] ?? []) { note in
                        noteCard(note)
                    }
                }
                ForEach(placed.detached) { note in
                    noteCard(note)
                }
            }
        }
        .navigationTitle(file.name)
        .navigationBarTitleDisplayMode(.inline)
        .overlay {
            if loading { ProgressView() }
        }
        .safeAreaInset(edge: .bottom) {
            selectionBar
        }
        .sheet(item: $composing) { draft in
            NoteComposerSheet(draft: draft) { kind, body in
                switch draft.editing {
                case .some(let note):
                    return await model.editNote(note, body: body)
                case .none:
                    guard let range = draft.range else { return false }
                    let saved = await model.addNote(file: file, range: range, kind: kind, body: body)
                    if saved { selection = nil }
                    return saved
                }
            }
        }
        .sheet(item: $sending) { note in
            SendNotesSheet(model: model, scope: .note, note: note, file: file)
        }
        .confirmationDialog(
            "Delete this note?",
            isPresented: Binding(
                get: { confirmDelete != nil },
                set: { if !$0 { confirmDelete = nil } }
            ),
            titleVisibility: .visible
        ) {
            Button("Delete", role: .destructive) {
                if let note = confirmDelete { Task { await model.delete(note) } }
                confirmDelete = nil
            }
        }
        .task { await load() }
    }

    private func isSelected(_ row: DiffRow) -> Bool {
        guard let selection else { return false }
        return (min(selection.anchor, selection.cursor)...max(selection.anchor, selection.cursor))
            .contains(row.id)
    }

    @ViewBuilder
    private var selectionBar: some View {
        if let selection, let range = noteRange(rows: rows, selection: selection) {
            HStack {
                Button("Clear") { self.selection = nil }
                Spacer()
                Button {
                    composing = NoteDraft(range: range, editing: nil)
                } label: {
                    Label("Comment on \(range.label)", systemImage: "text.bubble")
                        .font(.body.weight(.semibold))
                }
            }
            .padding()
            .background(.bar)
        } else if model.canWrite && !rows.isEmpty {
            Text("Tap a line to comment. Tap a second line to select a range.")
                .font(.caption)
                .foregroundStyle(.secondary)
                .frame(maxWidth: .infinity)
                .padding(.vertical, 8)
                .background(.bar)
        }
    }

    private func noteCard(_ note: DiffNote) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 8) {
                Text(note.kindTitle)
                    .font(.caption.weight(.semibold))
                Text(note.rangeLabel)
                    .font(.system(.caption, design: .monospaced))
                    .foregroundStyle(.secondary)
                if !note.isOpen {
                    Text((note.state ?? "").capitalized)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                if note.sentCount > 0 {
                    Text("sent \(note.sentCount)×")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                Spacer()
                if model.canWrite {
                    Menu {
                        Button("Send to agent", systemImage: "paperplane") { sending = note }
                        Button("Edit", systemImage: "pencil") {
                            composing = NoteDraft(
                                range: NoteRange(side: note.noteSide, start: note.start_line, end: note.lastLine),
                                editing: note
                            )
                        }
                        Button(
                            note.isResolved ? "Reopen" : "Resolve",
                            systemImage: note.isResolved ? "arrow.uturn.backward" : "checkmark"
                        ) {
                            Task { await model.setResolved(note, !note.isResolved) }
                        }
                        Button("Delete", systemImage: "trash", role: .destructive) {
                            confirmDelete = note
                        }
                    } label: {
                        Image(systemName: "ellipsis.circle")
                    }
                }
            }
            Text(note.body)
                .font(.subheadline)
                .foregroundStyle(note.isOpen ? .primary : .secondary)
        }
        .padding(10)
        .background(Color(.secondarySystemBackground), in: RoundedRectangle(cornerRadius: 10))
        .padding(.horizontal, 12)
        .padding(.vertical, 6)
    }

    private func load() async {
        loading = true
        defer { loading = false }
        do {
            let reply = try await model.patch(for: file)
            rows = diffRows(from: reply.hunks)
            truncated = reply.truncated ?? false
            binary = reply.file.binary ?? false
        } catch {
            loadError = error.localizedDescription
        }
    }
}

private struct DiffLineRow: View {
    let row: DiffRow
    let selected: Bool

    var body: some View {
        if row.kind == .hunk {
            Text(row.text)
                .font(.system(.caption, design: .monospaced))
                .foregroundStyle(.secondary)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.horizontal, 10)
                .padding(.vertical, 4)
                .background(Color(.secondarySystemBackground))
        } else {
            HStack(alignment: .top, spacing: 0) {
                gutter(row.old)
                gutter(row.new)
                Text(marker)
                    .foregroundStyle(markerColor)
                    .frame(width: 14)
                Text(row.text.isEmpty ? " " : row.text)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            .font(.system(.caption, design: .monospaced))
            .padding(.vertical, 1)
            .background(wash)
            .overlay(selected ? Color.accentColor.opacity(0.25) : Color.clear)
        }
    }

    private func gutter(_ number: Int?) -> some View {
        Text(number.map(String.init) ?? "")
            .foregroundStyle(.secondary)
            .frame(width: 34, alignment: .trailing)
            .padding(.trailing, 4)
    }

    private var marker: String {
        switch row.kind {
        case .addition: "+"
        case .deletion: "−"
        default: " "
        }
    }

    private var markerColor: Color {
        switch row.kind {
        case .addition: .green
        case .deletion: .red
        default: .secondary
        }
    }

    private var wash: Color {
        switch row.kind {
        case .addition: Color.green.opacity(0.15)
        case .deletion: Color.red.opacity(0.15)
        default: Color.clear
        }
    }
}

// MARK: - Composing and sending

/// A note being written, or an existing one being edited.
struct NoteDraft: Identifiable {
    let id = UUID()
    let range: NoteRange?
    let editing: DiffNote?
}

private struct NoteComposerSheet: View {
    let draft: NoteDraft
    /// Saves the note; true closes the sheet.
    let save: (NoteKind, String) async -> Bool

    @Environment(\.dismiss) private var dismiss
    @State private var kind: NoteKind
    @State private var text: String
    @State private var saving = false
    @FocusState private var focused: Bool

    init(draft: NoteDraft, save: @escaping (NoteKind, String) async -> Bool) {
        self.draft = draft
        self.save = save
        _kind = State(initialValue: NoteKind(rawValue: draft.editing?.kind ?? "") ?? .question)
        _text = State(initialValue: draft.editing?.body ?? "")
    }

    var body: some View {
        NavigationStack {
            Form {
                if draft.editing == nil {
                    Picker("Kind", selection: $kind) {
                        ForEach(NoteKind.allCases) { Text($0.title).tag($0) }
                    }
                    .pickerStyle(.segmented)
                }
                TextField("Leave a note for this change…", text: $text, axis: .vertical)
                    .lineLimit(3...10)
                    .focused($focused)
            }
            .navigationTitle(draft.range?.label ?? "Note")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { dismiss() }
                }
                ToolbarItem(placement: .confirmationAction) {
                    if saving {
                        ProgressView()
                    } else {
                        Button("Save") {
                            saving = true
                            Task {
                                let saved = await save(kind, text.trimmingCharacters(in: .whitespacesAndNewlines))
                                saving = false
                                if saved { dismiss() }
                            }
                        }
                        .disabled(text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                    }
                }
            }
            .onAppear { focused = true }
        }
        .presentationDetents([.medium, .large])
    }
}

/// Choose an agent and what to hand it.
struct SendNotesSheet: View {
    let model: DiffReviewModel
    @State var scope: NoteSendScope
    let note: DiffNote?
    let file: DiffFileRow?

    @Environment(AppStore.self) private var store
    @Environment(\.dismiss) private var dismiss
    @State private var sending = false

    /// Agents on this machine, the one the review was opened from first.
    private var targets: [Agent] {
        let all = store.agents.filter { $0.machine == model.machine }
        let preferred = model.agent?.workspace_id
        return all.sorted { lhs, rhs in
            let left = lhs.workspace_id == preferred, right = rhs.workspace_id == preferred
            return left && !right
        }
    }

    private var scopes: [NoteSendScope] {
        NoteSendScope.allCases.filter { scope in
            switch scope {
            case .note: note != nil
            case .file: file != nil
            case .all: true
            }
        }
    }

    var body: some View {
        NavigationStack {
            List {
                Section {
                    Picker("Send", selection: $scope) {
                        ForEach(scopes) { Text($0.title).tag($0) }
                    }
                    .pickerStyle(.segmented)
                }
                Section("Send to") {
                    if targets.isEmpty {
                        Text("No live agent. Start one, then send.")
                            .foregroundStyle(.secondary)
                    }
                    ForEach(targets) { target in
                        Button {
                            send(to: target)
                        } label: {
                            HStack {
                                Text(target.title).foregroundStyle(.primary)
                                Spacer()
                                Text(target.status ?? "")
                                    .font(.caption)
                                    .foregroundStyle(.secondary)
                            }
                        }
                        .disabled(sending)
                    }
                }
                if let status = model.status {
                    Section { Text(status).font(.footnote).foregroundStyle(.secondary) }
                }
            }
            .navigationTitle("Send notes")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { dismiss() }
                }
            }
        }
        .presentationDetents([.medium])
    }

    private func send(to target: Agent) {
        sending = true
        Task {
            let sent = await model.send(scope: scope, note: note, file: file, to: target)
            sending = false
            if sent { dismiss() }
        }
    }
}
