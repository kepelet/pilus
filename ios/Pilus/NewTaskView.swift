import SwiftUI

/// Sheet for spawning an ORCH workspace worker. `agent.start` is not reachable
/// through the private gateway, so the only remote spawn path is
/// `automation.create` + `automation.run`; agent kind is a text field because
/// `server.agent_manifests` is scope-denied remotely.
struct NewTaskView: View {
    @Environment(AppStore.self) private var store
    @Environment(\.dismiss) private var dismiss

    @State private var workspaceID = ""
    @State private var agent = "claude"
    @State private var title = ""
    @State private var prompt = ""
    @State private var sending = false
    @State private var sendError: String?

    private var canSend: Bool {
        !workspaceID.isEmpty && !agent.isEmpty && !title.isEmpty && !prompt.isEmpty && !sending
    }

    var body: some View {
        NavigationStack {
            Form {
                if let sendError {
                    Text(sendError)
                        .font(.footnote)
                        .foregroundStyle(.red)
                }
                Section("Workspace") {
                    Picker("Workspace", selection: $workspaceID) {
                        ForEach(store.workspaces) { workspace in
                            Text(workspace.title).tag(workspace.workspace_id)
                        }
                    }
                }
                Section("Agent") {
                    TextField("Agent kind", text: $agent)
                        .autocorrectionDisabled()
                        .textInputAutocapitalization(.never)
                    TextField("Title", text: $title)
                }
                Section("Prompt") {
                    TextField("What should the agent do?", text: $prompt, axis: .vertical)
                        .autocorrectionDisabled()
                        .lineLimit(3...6)
                }
            }
            .navigationTitle("New Task")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { dismiss() }
                }
                ToolbarItem(placement: .confirmationAction) {
                    if sending {
                        ProgressView()
                    } else {
                        Button("Run") { send() }
                            .buttonStyle(.borderedProminent)
                            .disabled(!canSend)
                    }
                }
            }
            .onAppear {
                if workspaceID.isEmpty {
                    workspaceID = store.workspaces.first(where: { $0.active == true })?.workspace_id
                        ?? store.workspaces.first?.workspace_id
                        ?? ""
                }
            }
        }
    }

    private func send() {
        sending = true
        sendError = nil
        Task {
            do {
                try await store.createTask(
                    workspaceID: workspaceID, agent: agent, title: title, prompt: prompt
                )
                await store.refresh()
                dismiss()
            } catch {
                sendError = error.localizedDescription
                sending = false
            }
        }
    }
}
