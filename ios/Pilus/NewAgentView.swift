import SwiftUI

/// The full `hub.run` sheet: pick an agent kind, an existing workspace or a
/// path browsed via `hub.folders`, then a prompt. The composer's quick path
/// covers "agent into an existing workspace"; this sheet is for a new
/// workspace on a folder that is not open yet.
struct NewAgentView: View {
    @Environment(AppStore.self) private var store
    @Environment(\.dismiss) private var dismiss

    @State private var agent = ""
    @State private var model: String?
    /// nil means "new workspace at `path`".
    @State private var workspaceID: String?
    @State private var newWorkspace = false
    /// Machine a new workspace opens on; an existing workspace carries its
    /// own tag and needs no picker.
    @State private var browseMachine = "local"
    @State private var path = "~"
    @State private var listing: HubFolders?
    @State private var prompt = ""
    @State private var running = false
    @State private var sendError: String?

    /// The machine the run targets: the workspace's own when one is picked,
    /// the browse selection otherwise.
    private var targetMachine: String {
        if !newWorkspace, let workspace = store.workspaces.first(where: { $0.workspace_id == workspaceID }) {
            return workspace.machine
        }
        return browseMachine
    }

    private var availableKinds: [HubAgentKind] {
        store.kinds(for: targetMachine)
    }

    private var currentKind: HubAgentKind? {
        availableKinds.first(where: { $0.kind == agent })
    }

    private var canSend: Bool {
        !agent.isEmpty
            && (newWorkspace ? !path.isEmpty : workspaceID != nil)
            && !prompt.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            && !running
    }

    var body: some View {
        NavigationStack {
            Form {
                if let sendError {
                    Text(sendError)
                        .font(.footnote)
                        .foregroundStyle(.red)
                }
                Section("Agent") {
                    Picker("Agent", selection: $agent) {
                        ForEach(availableKinds) { kind in
                            Text(kind.title).tag(kind.kind)
                        }
                    }
                    if let models = currentKind?.models, !models.isEmpty {
                        Picker("Model", selection: Binding(
                            get: { model ?? currentKind?.defaultModel ?? "" },
                            set: { model = $0 }
                        )) {
                            ForEach(models) { entry in
                                Text(entry.name).tag(entry.name)
                            }
                        }
                    }
                }
                Section("Workspace") {
                    Picker("Target", selection: $newWorkspace) {
                        Text("Existing workspace").tag(false)
                        Text("New workspace…").tag(true)
                    }
                    .pickerStyle(.segmented)
                    if newWorkspace {
                        if store.machineOrder.count > 1 {
                            Picker("Machine", selection: $browseMachine) {
                                ForEach(store.machineOrder, id: \.self) { id in
                                    Text(store.machineTitle(id)).tag(id)
                                }
                            }
                        }
                        folderBrowser
                    } else {
                        Picker("Workspace", selection: Binding(
                            get: { workspaceID ?? "" },
                            set: { workspaceID = $0 }
                        )) {
                            ForEach(store.workspaces) { workspace in
                                Text(workspace.title).tag(workspace.workspace_id)
                            }
                        }
                    }
                }
                Section("Prompt") {
                    TextField("What should the agent do?", text: $prompt, axis: .vertical)
                        .autocorrectionDisabled()
                        .lineLimit(3...6)
                }
            }
            .navigationTitle("New Agent")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { dismiss() }
                }
                ToolbarItem(placement: .confirmationAction) {
                    if running {
                        ProgressView()
                    } else {
                        Button("Run") { send() }
                            .buttonStyle(.borderedProminent)
                            .disabled(!canSend)
                    }
                }
            }
            .onAppear {
                browseMachine = store.machineOrder.first ?? "local"
                if agent.isEmpty, let first = availableKinds.first {
                    agent = first.kind
                    model = first.defaultModel
                }
                if workspaceID == nil {
                    workspaceID = store.workspaces.first(where: { $0.active == true })?.workspace_id
                        ?? store.workspaces.first?.workspace_id
                }
                Task { await browse(path) }
            }
            .onChange(of: browseMachine) {
                listing = nil
                path = "~"
                Task { await browse(path) }
            }
        }
    }

    /// `hub.folders` listing with a path bar, up-navigation, and tap-to-pick
    /// on the folders it returns.
    @ViewBuilder
    private var folderBrowser: some View {
        HStack {
            Text(path)
                .font(.system(.footnote, design: .monospaced))
                .lineLimit(1)
                .truncationMode(.head)
            Spacer()
            if let parent = listing?.parent {
                Button("Up") { Task { await browse(parent) } }
                    .font(.footnote)
            }
        }
        if let folders = listing?.folders {
            ForEach(folders) { folder in
                Button {
                    path = folder.path
                    Task { await browse(folder.path) }
                } label: {
                    Label {
                        Text(folder.name)
                            .foregroundStyle(.primary)
                    } icon: {
                        Image(systemName: folder.git == true ? "folder.badge.gearshape" : "folder")
                            .foregroundStyle(.secondary)
                    }
                }
            }
            if folders.isEmpty {
                Text("No subfolders")
                    .font(.footnote)
                    .foregroundStyle(.secondary)
            }
        } else {
            ProgressView()
        }
    }

    private func browse(_ to: String) async {
        do {
            listing = try await store.folders(path: to, machine: browseMachine)
            path = listing?.root ?? to
        } catch {
            listing = nil
            sendError = error.localizedDescription
        }
    }

    private func send() {
        running = true
        sendError = nil
        let workspace = store.workspaces.first(where: { $0.workspace_id == workspaceID })
        Task {
            do {
                _ = try await store.runAgent(
                    agent: agent,
                    path: newWorkspace ? path : (workspace?.cwd ?? workspace?.terminal_cwd ?? "~"),
                    prompt: prompt.trimmingCharacters(in: .whitespacesAndNewlines),
                    workspaceID: newWorkspace ? nil : workspaceID,
                    model: model ?? currentKind?.defaultModel,
                    machine: targetMachine
                )
                await store.refresh()
                dismiss()
            } catch {
                sendError = error.localizedDescription
                running = false
            }
        }
    }
}
