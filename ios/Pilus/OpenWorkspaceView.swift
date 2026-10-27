import SwiftUI

/// The "Open Workspace" sheet: browse a machine's folders via `hub.folders`
/// and send `workspace.open`, which opens the path as a workspace on the
/// daemon (or focuses it when already open). Operate-level, same gate as
/// the composer.
struct OpenWorkspaceView: View {
    @Environment(AppStore.self) private var store
    @Environment(\.dismiss) private var dismiss

    /// Told the folder and machine once the workspace is open.
    var onOpened: ((String, String) -> Void)? = nil

    @State private var machine = "local"
    @State private var path = "~"
    @State private var listing: HubFolders?
    @State private var browsing = false
    @State private var opening = false
    @State private var sendError: String?

    var body: some View {
        NavigationStack {
            Form {
                if let sendError {
                    Text(sendError)
                        .font(.footnote)
                        .foregroundStyle(.red)
                }
                if store.machineOrder.count > 1 {
                    Picker("Machine", selection: $machine) {
                        ForEach(store.machineOrder, id: \.self) { id in
                            Text(store.machineTitle(id)).tag(id)
                        }
                    }
                }
                Section("Folder") {
                    folderBrowser
                }
            }
            .navigationTitle("Open Workspace")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { dismiss() }
                }
                ToolbarItem(placement: .confirmationAction) {
                    if opening {
                        ProgressView()
                    } else {
                        Button("Open") { open() }
                            .buttonStyle(.borderedProminent)
                    }
                }
            }
            .onAppear {
                machine = store.machineOrder.first ?? "local"
                Task { await browse(path) }
            }
            .onChange(of: machine) {
                listing = nil
                path = "~"
                Task { await browse(path) }
            }
        }
    }

    /// `hub.folders` listing with a path bar, up-navigation, and tap-to-dive
    /// on the folders it returns — same browser as New Agent.
    @ViewBuilder
    private var folderBrowser: some View {
        HStack {
            Text(path)
                .font(.system(.footnote, design: .monospaced))
                .lineLimit(1)
                .truncationMode(.head)
            Spacer()
            if browsing {
                ProgressView()
                    .controlSize(.small)
            }
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
        } else if browsing {
            ProgressView()
        }
    }

    private func browse(_ to: String) async {
        browsing = true
        defer { browsing = false }
        do {
            listing = try await store.folders(path: to, machine: machine)
            path = listing?.root ?? to
        } catch {
            listing = nil
            sendError = error.localizedDescription
        }
    }

    private func open() {
        opening = true
        sendError = nil
        Task {
            do {
                try await store.openWorkspace(path: path, machine: machine)
                await store.refresh()
                onOpened?(path, machine)
                dismiss()
            } catch {
                sendError = error.localizedDescription
                opening = false
            }
        }
    }
}
