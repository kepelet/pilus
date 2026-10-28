import SwiftUI

/// The toolbar compose sheet — a roomier take on the home composer. Header:
/// just close on the left; the prompt gets the body; the agent, model, effort
/// and workspace pickers sit in a bar above the keyboard with send beside them.
struct ComposeSheetView: View {
    @Environment(AppStore.self) private var store
    @Environment(\.dismiss) private var dismiss
    /// Shared with the home composer so text survives switching surfaces.
    var draft: Binding<String>
    /// The pickers are shared too — a kind/model/workspace chosen in the
    /// mini composer is what the expanded sheet opens with.
    var agent: Binding<String>
    var model: Binding<String?>
    /// `nil` leaves the reasoning level to the agent's own default.
    var effort: Binding<String?>
    var workspaceID: Binding<String?>
    /// Called with the agent `hub.run` started so the parent can push detail.
    var onLaunched: (Agent) -> Void

    @State private var running = false
    @State private var showingOpenWorkspace = false
    @State private var sendError: String?
    @FocusState private var editorFocused: Bool

    var body: some View {
        VStack(spacing: 0) {
            HStack {
                Button { dismiss() } label: {
                    Image(systemName: "xmark")
                        .font(.body.weight(.medium))
                        .foregroundStyle(.primary)
                        .frame(width: 36, height: 36)
                        .modifier(GlassCircle())
                        .contentShape(Circle())
                }
                .buttonStyle(.plain)
                .accessibilityLabel("Close")
                Spacer()
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 12)

            if let sendError {
                Text(sendError)
                    .font(.footnote)
                    .foregroundStyle(.red)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.horizontal, 18)
                    .padding(.bottom, 4)
            }

            TextEditor(text: draft)
                .font(.body)
                .autocorrectionDisabled()
                .textInputAutocapitalization(.never)
                .focused($editorFocused)
                .scrollContentBackground(.hidden)
                .padding(.horizontal, 14)
                .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
                .overlay(alignment: .topLeading) {
                    if draft.wrappedValue.isEmpty {
                        Text("What should we build today?")
                            .foregroundStyle(.secondary)
                            .padding(.horizontal, 19)
                            .padding(.vertical, 8)
                            .allowsHitTesting(false)
                    }
                }

            HStack(spacing: 10) {
                // Four chips can outrun a phone's width; they scroll while
                // send stays put at the corner.
                ScrollView(.horizontal, showsIndicators: false) {
                    HStack(spacing: 8) {
                        agentPicker
                        modelPicker
                        effortPicker
                        workspacePicker
                    }
                    .padding(.vertical, 2)
                }
                sendButton
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 10)
            // Menus inherit the accent tint inside a toolbar row; pin the
            // pickers to monochrome like the rest of the UI.
            .tint(.primary)
        }
        .presentationDetents([.large])
        .presentationDragIndicator(.hidden)
        .sheet(isPresented: $showingOpenWorkspace) {
            OpenWorkspaceView { path, machine in
                if let opened = store.workspaces.first(where: {
                    $0.machine == machine && ($0.cwd == path || $0.terminal_cwd == path)
                }) {
                    workspaceID.wrappedValue = opened.workspace_id
                }
            }
            .presentationDragIndicator(.hidden)
        }
        .onAppear {
            applyDefaults()
            if availableKinds.isEmpty || store.missingAgentKinds {
                Task { await store.loadAgentKinds() }
            }
            Task { editorFocused = true }
        }
        .onChange(of: availableKinds.map(\.kind)) { applyDefaults() }
        .onChange(of: store.workspaces.count) { applyDefaults() }
        .onChange(of: agent.wrappedValue) {
            model.wrappedValue = currentKind?.defaultModel
            effort.wrappedValue = nil
        }
        .onChange(of: model.wrappedValue) { effort.wrappedValue = nil }
        .onChange(of: workspaceID.wrappedValue) {
            if !availableKinds.isEmpty, !availableKinds.contains(where: { $0.kind == agent.wrappedValue }) {
                if let first = availableKinds.first {
                    agent.wrappedValue = first.kind
                    model.wrappedValue = first.defaultModel
                }
            }
        }
    }

    // MARK: - Selection (mirrors HomeComposerView)

    private var currentMachine: String {
        currentWorkspace?.machine ?? store.machineOrder.first ?? "local"
    }

    private var availableKinds: [HubAgentKind] {
        store.kinds(for: currentMachine)
    }

    private var currentKind: HubAgentKind? {
        availableKinds.first(where: { $0.kind == agent.wrappedValue })
    }

    private var currentWorkspace: Workspace? {
        store.workspaces.first(where: { $0.workspace_id == workspaceID.wrappedValue })
    }

    private var spansMachines: Bool {
        Set(store.workspaces.map(\.machine)).count > 1
    }

    private func applyDefaults() {
        if agent.wrappedValue.isEmpty, let first = availableKinds.first {
            agent.wrappedValue = first.kind
            model.wrappedValue = first.defaultModel
        }
        if workspaceID.wrappedValue == nil {
            workspaceID.wrappedValue = store.workspaces.first(where: { $0.active == true })?.workspace_id
                ?? store.workspaces.first?.workspace_id
        }
    }

    private var agentPicker: some View {
        Menu {
            ForEach(availableKinds) { kind in
                Button { agent.wrappedValue = kind.kind } label: {
                    Label {
                        Text(kind.title)
                    } icon: {
                        if agent.wrappedValue == kind.kind {
                            Image(systemName: "checkmark")
                        }
                    }
                }
            }
        } label: {
            pickerLabel(agent.wrappedValue.isEmpty ? "agent" : agent.wrappedValue,
                        agentKind: agent.wrappedValue.isEmpty ? nil : agent.wrappedValue)
        }
    }

    /// Only shown for kinds the hub reports a model catalog for; older hubs
    /// have no `models` field and this picker simply doesn't appear.
    private var modelPicker: some View {
        Group {
            if let models = currentKind?.models, !models.isEmpty {
                Menu {
                    ForEach(models) { entry in
                        Button { model.wrappedValue = entry.name } label: {
                            Label {
                                Text(entry.name)
                            } icon: {
                                if model.wrappedValue == entry.name {
                                    Image(systemName: "checkmark")
                                }
                            }
                        }
                    }
                } label: {
                    pickerLabel(model.wrappedValue ?? "model", systemImage: "cpu")
                }
            }
        }
    }

    /// Reasoning level for the chosen model, when the catalog lists any.
    @ViewBuilder
    private var effortPicker: some View {
        if let entry = currentKind?.entry(model.wrappedValue), let levels = entry.efforts, !levels.isEmpty {
            Menu {
                ForEach(["Default"] + levels, id: \.self) { level in
                    Button { effort.wrappedValue = level == "Default" ? nil : level } label: {
                        Label {
                            Text(level)
                        } icon: {
                            if (effort.wrappedValue ?? "Default") == level {
                                Image(systemName: "checkmark")
                            }
                        }
                    }
                }
            } label: {
                pickerLabel(effort.wrappedValue ?? entry.default_effort ?? "effort",
                            systemImage: "brain")
            }
        }
    }

    private var workspacePicker: some View {
        Menu {
            ForEach(store.workspaces) { workspace in
                Button { workspaceID.wrappedValue = workspace.workspace_id } label: {
                    Label {
                        Text(spansMachines
                             ? "\(workspace.title) · \(store.machineTitle(workspace.machine))"
                             : workspace.title)
                    } icon: {
                        if workspaceID.wrappedValue == workspace.workspace_id {
                            Image(systemName: "checkmark")
                        }
                    }
                }
            }
            if store.canRunAgents {
                Divider()
                Button("Open Workspace") { showingOpenWorkspace = true }
            }
        } label: {
            pickerLabel(currentWorkspace?.title ?? "workspace", asset: "lucide-server")
        }
    }

    private func pickerLabel(_ title: String, systemImage: String? = nil,
                             asset: String? = nil, agentKind: String? = nil) -> some View {
        HStack(spacing: 5) {
            if let agentKind {
                AgentIconView(kind: agentKind, size: 14)
            } else if let asset {
                Image(asset)
                    .resizable()
                    .renderingMode(.template)
                    .aspectRatio(contentMode: .fit)
                    .frame(width: 11, height: 11)
            } else if let systemImage {
                Image(systemName: systemImage)
                    .font(.system(size: 11))
            }
            Text(title)
                .font(.footnote)
                .lineLimit(1)
            Image(systemName: "chevron.down")
                .font(.system(size: 8, weight: .semibold))
                .foregroundStyle(.secondary)
        }
        .foregroundStyle(.primary)
        .padding(.horizontal, 12)
        .padding(.vertical, 7)
        .modifier(GlassChip())
    }

    private var sendButton: some View {
        Button(action: send) {
            if running {
                ProgressView()
                    .frame(width: 32, height: 32)
            } else {
                SendArrow(enabled: canSend, size: 36)
            }
        }
        .disabled(!canSend || running)
        .accessibilityLabel("Send")
    }

    private var canSend: Bool {
        !running && currentKind != nil
    }

    private func send() {
        let text = draft.wrappedValue.trimmingCharacters(in: .whitespacesAndNewlines)
        guard canSend, !running else { return }
        running = true
        sendError = nil
        let workspace = currentWorkspace
        Task {
            do {
                let result = try await store.runAgent(
                    agent: agent.wrappedValue,
                    path: workspace?.cwd ?? workspace?.terminal_cwd ?? "~",
                    prompt: text,
                    workspaceID: workspace?.workspace_id,
                    model: model.wrappedValue,
                    effort: effort.wrappedValue,
                    machine: currentMachine
                )
                draft.wrappedValue = ""
                await store.refresh()
                if let pane = result.pane {
                    onLaunched(Agent(
                        pane: pane,
                        agent: agent.wrappedValue,
                        name: result.name,
                        agent_session_title: nil,
                        status: "running",
                        workspace_id: workspace?.workspace_id,
                        workspace_name: workspace?.title,
                        cwd: workspace?.cwd,
                        branch: nil,
                        focused: true,
                        machine: currentMachine
                    ))
                }
            } catch {
                sendError = error.localizedDescription
            }
            running = false
        }
    }
}

/// Liquid Glass capsule for the picker chips, material + hairline below
/// iOS 26.
private struct GlassChip: ViewModifier {
    func body(content: Content) -> some View {
        if #available(iOS 26.0, *) {
            content.glassEffect(.regular.interactive(), in: .capsule)
        } else {
            content
                .background(.regularMaterial, in: .capsule)
                .overlay(Capsule().strokeBorder(.quaternary))
        }
    }
}

/// Liquid Glass circle where available, material + hairline elsewhere.
private struct GlassCircle: ViewModifier {
    func body(content: Content) -> some View {
        if #available(iOS 26.0, *) {
            content.glassEffect(.regular.interactive(), in: .circle)
        } else {
            content
                .background(.regularMaterial, in: Circle())
                .overlay(Circle().strokeBorder(.quaternary))
        }
    }
}
