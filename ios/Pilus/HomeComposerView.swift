import SwiftUI

/// The launch composer at the foot of Home: a tall text field with the
/// agent/model/workspace pickers in a row beneath it and the send button at
/// the bottom-right — fire `hub.run` and land on the new agent's pane.
/// The parent only shows it at the operate level; it folds the card away
/// when the user taps outside it.
///
/// Pickers are transparent text menus (no capsule, no glass) to match the
/// list above; the only container is the glass card itself.
struct HomeComposerView: View {
    @Environment(AppStore.self) private var store
    /// Owned by the parent so a tap outside the card can drop the keyboard.
    var promptFocused: FocusState<Bool>.Binding
    /// The draft text — owned by the parent so folding the card away does not
    /// lose whatever the user was typing.
    var draft: Binding<String>
    /// Called with the agent `hub.run` started so the parent can push detail.
    var onLaunched: (Agent) -> Void

    @State private var agent = ""
    @State private var model: String?
    /// Machine-tagged selection: `workspace_id` alone is enough, the machine
    /// tag rides on the workspace row itself.
    @State private var workspaceID: String?
    @State private var running = false
    @State private var sendError: String?

    var body: some View {
        VStack(spacing: 4) {
            if let sendError {
                Text(sendError)
                    .font(.footnote)
                    .foregroundStyle(.red)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.bottom, 2)
            }
            VStack(alignment: .leading, spacing: 8) {
                TextField("Plan, ask, build…", text: draft, axis: .vertical)
                    .lineLimit(3...5)
                    .textFieldStyle(.plain)
                    .focused(promptFocused)
                    // Prompts reach the agent verbatim.
                    .autocorrectionDisabled()
                    .textInputAutocapitalization(.never)
                HStack(spacing: 16) {
                    agentPicker
                    modelPicker
                    workspacePicker
                    Spacer()
                    sendButton
                }
                .tint(.primary)
            }
            .padding(.horizontal, 14)
            .padding(.top, 12)
            .padding(.bottom, 10)
            .modifier(LiquidGlassPanel())
        }
        .onAppear(perform: applyDefaults)
        .onChange(of: availableKinds.map(\.kind)) { applyDefaults() }
        .onChange(of: store.workspaces.count) { applyDefaults() }
        .onChange(of: agent) { model = currentKind?.defaultModel }
        .onChange(of: workspaceID) {
            // The workspace choice pins the machine the run and the kind
            // catalog both come from; re-pick the kind if the machine
            // changed it away.
            if !availableKinds.isEmpty, !availableKinds.contains(where: { $0.kind == agent }) {
                if let first = availableKinds.first {
                    agent = first.kind
                    model = first.defaultModel
                }
            }
        }
    }

    /// The machine the workspace picker currently resolves to — runs and
    /// kind catalogs both follow it.
    private var currentMachine: String {
        currentWorkspace?.machine ?? store.machineOrder.first ?? "local"
    }

    private var availableKinds: [HubAgentKind] {
        store.kinds(for: currentMachine)
    }

    private var currentKind: HubAgentKind? {
        availableKinds.first(where: { $0.kind == agent })
    }

    private var currentWorkspace: Workspace? {
        store.workspaces.first(where: { $0.workspace_id == workspaceID })
    }

    private func applyDefaults() {
        if agent.isEmpty, let first = availableKinds.first {
            agent = first.kind
            model = first.defaultModel
        }
        if workspaceID == nil {
            workspaceID = store.workspaces.first(where: { $0.active == true })?.workspace_id
                ?? store.workspaces.first?.workspace_id
        }
    }

    // MARK: - Pickers (transparent text menus)

    private var agentPicker: some View {
        Menu {
            ForEach(availableKinds) { kind in
                Button { agent = kind.kind } label: {
                    Label {
                        Text(kind.title)
                    } icon: {
                        if agent == kind.kind {
                            Image(systemName: "checkmark")
                        }
                    }
                }
            }
        } label: {
            pickerLabel(agent.isEmpty ? "agent" : agent, agentKind: agent.isEmpty ? nil : agent)
        }
    }

    /// Only shown for kinds the hub reports a model catalog for; older hubs
    /// have no `models` field and this picker simply doesn't appear.
    @ViewBuilder
    private var modelPicker: some View {
        if let models = currentKind?.models, !models.isEmpty {
            Menu {
                ForEach(models) { entry in
                    Button { model = entry.name } label: {
                        Label(entry.name,
                              systemImage: model == entry.name ? "checkmark" : "cpu")
                    }
                }
            } label: {
                pickerLabel(model ?? "model", systemImage: "cpu")
            }
        }
    }

    /// Workspaces from every machine; the machine name is appended when the
    /// list spans more than one so two same-named folders stay distinct.
    private var workspacePicker: some View {
        Menu {
            ForEach(store.workspaces) { workspace in
                Button { workspaceID = workspace.workspace_id } label: {
                    Label {
                        Text(spansMachines
                             ? "\(workspace.title) · \(store.machineTitle(workspace.machine))"
                             : workspace.title)
                    } icon: {
                        if workspaceID == workspace.workspace_id {
                            Image(systemName: "checkmark")
                        }
                    }
                }
            }
        } label: {
            pickerLabel(currentWorkspace?.title ?? "workspace")
        }
    }

    private var spansMachines: Bool {
        Set(store.workspaces.map(\.machine)).count > 1
    }

    private func pickerLabel(_ title: String, systemImage: String? = nil, agentKind: String? = nil) -> some View {
        HStack(spacing: 5) {
            if let agentKind {
                AgentIconView(kind: agentKind, size: 14)
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
    }

    private var sendButton: some View {
        Button(action: send) {
            if running {
                ProgressView()
                    .frame(width: 28, height: 28)
            } else {
                SendArrow(enabled: canSend, size: 28)
            }
        }
        .disabled(!canSend || running)
        .accessibilityLabel("Run agent")
    }

    private var canSend: Bool {
        !agent.isEmpty && !draft.wrappedValue.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
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
                    agent: agent,
                    path: workspace?.cwd ?? workspace?.terminal_cwd ?? "~",
                    prompt: text,
                    workspaceID: workspace?.workspace_id,
                    model: model,
                    machine: currentMachine
                )
                draft.wrappedValue = ""
                await store.refresh()
                if let pane = result.pane {
                    onLaunched(Agent(
                        pane: pane,
                        agent: agent,
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

/// Liquid Glass where available, material + hairline elsewhere — the card
/// shape shared by the composer and the primary buttons.
struct LiquidGlassPanel: ViewModifier {
    var radius: CGFloat = 20
    /// Primary action styling: accent-tinted glass, white label.
    var prominent: Bool = false

    func body(content: Content) -> some View {
        if #available(iOS 26.0, *) {
            if prominent {
                content.glassEffect(.regular.tint(.blue).interactive(), in: .rect(cornerRadius: radius))
            } else {
                content.glassEffect(.regular.interactive(), in: .rect(cornerRadius: radius))
            }
        } else {
            content
                .background(prominent ? AnyShapeStyle(Color.accentColor) : AnyShapeStyle(.regularMaterial),
                            in: .rect(cornerRadius: radius))
                .overlay(RoundedRectangle(cornerRadius: radius).strokeBorder(.quaternary))
        }
    }
}

/// The round send button every composer shares: blue like the other primary
/// actions while there is something to send, a quiet glass disc otherwise.
struct SendArrow: View {
    var enabled: Bool
    var size: CGFloat

    var body: some View {
        Image(systemName: "arrow.up")
            .font(.system(size: size * 0.46, weight: .semibold))
            .foregroundStyle(enabled ? Color.white : Color.secondary)
            .frame(width: size, height: size)
            .modifier(LiquidGlassPanel(radius: size / 2, prominent: enabled))
            .opacity(enabled ? 1 : 0.5)
    }
}
