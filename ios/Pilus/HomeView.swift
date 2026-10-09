import SwiftUI

/// The single screen: host header, then every workspace across every
/// machine with its agents grouped beneath, then orphan agents and tasks.
/// Deliberately not a `List` — no separators, disclosure chevrons, or
/// section chrome; rows are plain text on the system background and the
/// only accent is monochrome.
struct HomeView: View {
    @Environment(AppStore.self) private var store
    @State private var navPath = NavigationPath()
    /// The draft lives here so closing the compose sheet never throws
    /// text away.
    @State private var composerDraft = ""
    @State private var showingNewTask = false
    @State private var showingOpenWorkspace = false
    @State private var showingPairing = false
    @State private var showingSettings = false
    @State private var showingCompose = false
    @State private var openingTaskID: String?
    /// The floating composer's three states ride on this: unfocused (one
    /// line), focused (taller), maximized (the compose sheet).
    @FocusState private var composerFocused: Bool
    @State private var composerSending = false
    @State private var composerError: String?

    /// In-stack destinations that are not an `Agent`.
    private enum HomeRoute: Hashable {
        case notifications
    }
    @AppStorage("home.machines.expanded") private var machinesExpanded = true
    @AppStorage("home.agents.expanded") private var agentsExpanded = true
    @AppStorage("home.tasks.expanded") private var tasksExpanded = true

    /// A top-level workspace row plus the linked worktrees hanging under it.
    private struct WorkspaceNode {
        let workspace: Workspace
        var worktrees: [Workspace] = []
    }

    /// Workspaces grouped under their machine in `hub.machines` order,
    /// each group sorted by the daemon's own display position. Worktree
    /// workspaces fold into their repo's workspace as indented children.
    private var machineGroups: [(machine: String, workspaces: [WorkspaceNode])] {
        store.machineOrder.compactMap { id in
            let ws = store.workspaces
                .filter { $0.machine == id && !isHomeWorkspace($0) }
                .sorted {
                    (Int($0.display_position ?? "") ?? 0) < (Int($1.display_position ?? "") ?? 0)
                }
            let nodes = groupWorktrees(ws)
            return nodes.isEmpty ? nil : (id, nodes)
        }
    }

    /// The daemon's linked-worktree convention: checkouts live at
    /// `$LUVUS_HOME/worktrees/<repo>/<branch>`, so a workspace rooted under
    /// `.luvus*/worktrees/` is a child of the workspace holding that repo's
    /// main checkout. Returns the repo slug, nil when it isn't a worktree.
    private func worktreeRepo(_ workspace: Workspace) -> String? {
        for candidate in [workspace.cwd, workspace.terminal_cwd] {
            guard let path = candidate,
                  // `.luvus` prefix also covers alternate homes like
                  // `.luvus-dev`; the worktrees root sits directly under it.
                  let home = path.range(of: "/.luvus"),
                  let marker = path.range(of: "/worktrees/",
                                          range: home.upperBound..<path.endIndex)
            else { continue }
            let tail = path[marker.upperBound...]
            let repo = tail.prefix(while: { $0 != "/" })
            if !repo.isEmpty, tail.count > repo.count + 1 {
                return String(repo)
            }
        }
        return nil
    }

    /// Fold worktree workspaces under their repo's workspace; a worktree whose
    /// repo isn't open stays a top-level row so it never goes missing.
    private func groupWorktrees(_ workspaces: [Workspace]) -> [WorkspaceNode] {
        var nodes: [WorkspaceNode] = []
        var children: [(workspace: Workspace, repo: String)] = []
        for workspace in workspaces {
            if let repo = worktreeRepo(workspace) {
                children.append((workspace, repo))
            } else {
                nodes.append(WorkspaceNode(workspace: workspace))
            }
        }
        for child in children {
            if let index = nodes.firstIndex(where: { lastPathComponent($0.workspace) == child.repo }) {
                nodes[index].worktrees.append(child.workspace)
            } else {
                nodes.append(WorkspaceNode(workspace: child.workspace))
            }
        }
        return nodes
    }

    private func lastPathComponent(_ workspace: Workspace) -> String? {
        (workspace.cwd ?? workspace.terminal_cwd)?
            .split(separator: "/")
            .last
            .map(String.init)
    }

    /// Bare home directories carry no context — they clutter the list, so
    /// they hide. Matches `~` and a resolved home path alike.
    private func isHomeWorkspace(_ workspace: Workspace) -> Bool {
        workspace.title == "~" || workspace.cwd == "~" || workspace.cwd == "/"
    }

    /// The machine group header only earns its row when the list actually
    /// spans more than one machine.
    private var spansMachines: Bool {
        machineGroups.count > 1
    }

    /// Agents `agent.list` attributes to this workspace — by id first, then by
    /// display name for replies that only carry one of them; always same
    /// machine, since pane ids repeat across daemons.
    private func agents(in workspace: Workspace) -> [Agent] {
        store.agents.filter {
            $0.machine == workspace.machine
                && ($0.workspace_id == workspace.workspace_id
                    || ($0.workspace_id == nil && $0.workspace_name == workspace.title))
        }
    }

    private var orphanAgents: [Agent] {
        store.agents.filter { agent in
            !store.workspaces.contains {
                // Hidden home workspaces can't hold rows; their agents
                // fall through to the flat Agents section.
                !isHomeWorkspace($0)
                    && $0.machine == agent.machine
                    && ($0.workspace_id == agent.workspace_id || $0.title == agent.workspace_name)
            }
        }
    }

    var body: some View {
        NavigationStack(path: $navPath) {
            topContent
            // The hero bleeds to the screen's top edge, behind a bar that
            // draws no material of its own — logo and bell sit bare on it.
            .ignoresSafeArea(.container, edges: .top)
            .toolbarBackground(.hidden, for: .navigationBar)
            .refreshable { await store.refresh() }
            // A tap anywhere outside the composer resigns it — rows and
            // section headers keep their own handlers.
            .onTapGesture { composerFocused = false }
            // Pushing a page (agent, notifications) drops the keyboard.
            .onChange(of: navPath.count) { composerFocused = false }
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                // The brand mark doubles as the menu trigger — tapping it
                // opens the host menu (New task / Pairing / Settings / …).
                if #available(iOS 26.0, *) {
                    ToolbarItem(placement: .topBarLeading) {
                        hostMenu
                            // fixedSize stops the toolbar capsule collapsing the
                            // label back to just the icon; sharedBackgroundVisibility
                            // drops the capsule itself — the wordmark draws bare,
                            // the icon flushed left to line up with the section
                            // labels' 20pt content edge below.
                            .fixedSize()
                            .padding(.leading, -10)
                    }
                    .sharedBackgroundVisibility(.hidden)
                } else {
                    ToolbarItem(placement: .topBarLeading) { hostMenu }
                }
                if #available(iOS 26.0, *) {
                    ToolbarItem(placement: .topBarTrailing) {
                        Button { navPath.append(HomeRoute.notifications) } label: {
                            Image(systemName: "bell")
                                .padding(8)
                                // Liquid glass circle — the same treatment
                                // the composer buttons get — while the
                                // wordmark stays bare over the hero.
                                .glassEffect(.regular.interactive(), in: .circle)
                        }
                        .environment(\.colorScheme, .dark)
                        .accessibilityLabel("Notifications")
                    }
                    .sharedBackgroundVisibility(.hidden)
                } else {
                    ToolbarItem(placement: .topBarTrailing) {
                        Button { navPath.append(HomeRoute.notifications) } label: {
                            Image(systemName: "bell")
                        }
                        .accessibilityLabel("Notifications")
                    }
                }
            }
            .navigationDestination(for: Agent.self) { agent in
                AgentDetailView(agent: agent)
            }
            .navigationDestination(for: HomeRoute.self) { route in
                switch route {
                case .notifications: NotificationsView()
                }
            }
            .sheet(isPresented: $showingNewTask) {
                NewTaskView()
            }
            .sheet(isPresented: $showingOpenWorkspace) {
                OpenWorkspaceView()
            }
            .sheet(isPresented: $showingPairing) {
                OnboardingView(onPaired: { showingPairing = false })
            }
            .sheet(isPresented: $showingSettings) {
                SettingsView()
            }
            .sheet(isPresented: $showingCompose) {
                ComposeSheetView(draft: $composerDraft,
                                 agent: $composerAgent,
                                 model: $composerModel,
                                 workspaceID: $composerWorkspaceID) { launched in
                    showingCompose = false
                    navPath.append(launched)
                }
            }
            .safeAreaInset(edge: .bottom) {
                // Optimistic while the first probe/refresh is still out —
                // the pickers skeleton inside until the catalogs land. If
                // the level comes back non-operate the composer folds away.
                if store.canRunAgents || (!store.loaded && store.error == nil) {
                    miniComposer
                }
            }
        }
    }

    /// The list, letting the hero bleed to the screen's top edge. iOS 26's
    /// top scroll-edge effect would frost it under the status area.
    @ViewBuilder
    private var topContent: some View {
        if #available(iOS 26.0, *) {
            scrollContent
                .scrollEdgeEffectHidden(true, for: .top)
                .toolbarBackgroundVisibility(.hidden, for: .navigationBar)
                // Dark bar scheme = light toolbar content and a light status
                // bar over the hero's dark scrim, in both appearances.
                .toolbarColorScheme(.dark, for: .navigationBar)
        } else {
            scrollContent
        }
    }

    private var scrollContent: some View {
        ScrollView {
            LazyVStack(alignment: .leading, spacing: 0) {
                heroHeader
                if let error = store.error {
                    Text(error)
                        .font(.footnote)
                        .foregroundStyle(.red)
                        .padding(.top, 8)
                }
                if store.workspaces.isEmpty && !store.loaded {
                    skeleton
                } else {
                    // An empty or unreachable host shows no section at
                    // all — no header, no hairline.
                    if !store.workspaces.isEmpty {
                        machinesSection
                    }
                    if !orphanAgents.isEmpty {
                        agentsSection
                    }
                    if !store.tasks.isEmpty {
                        tasksSection
                    }
                }
            }
            .padding(.horizontal, 20)
            .padding(.top, 12)
            .padding(.bottom, 16)
        }
    }

    /// The floating composer. Unfocused it is a one-line capsule — a field
    /// vertically centered beside the agent-view send button. Focusing grows
    /// it into a card: the prompt takes the top, the agent/model/workspace
    /// pickers and send sit on the bottom row. While tall, a flick up
    /// maximizes it into the compose sheet and a flick down settles it back.
    /// The draft and the pickers' choices are shared with the sheet.
    private var miniComposer: some View {
        VStack(spacing: 0) {
            if let composerError {
                Text(composerError)
                    .font(.footnote)
                    .foregroundStyle(.red)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.bottom, 4)
            }
            VStack(alignment: .leading, spacing: composerFocused ? 10 : 0) {
                HStack(alignment: composerFocused ? .top : .center, spacing: 8) {
                    TextField("Plan, ask, build…", text: $composerDraft, axis: .vertical)
                        .lineLimit(composerFocused ? 2...6 : 1...1)
                        .textFieldStyle(.plain)
                        .focused($composerFocused)
                        // Prompts reach the agent verbatim.
                        .autocorrectionDisabled()
                        .textInputAutocapitalization(.never)
                    if !composerFocused {
                        composerSendButton
                    }
                }
                if composerFocused {
                    HStack(spacing: 16) {
                        if composerKinds.isEmpty && !store.loaded {
                            pickerSkeleton
                            pickerSkeleton
                        } else {
                            composerAgentPicker
                            composerModelPicker
                        }
                        if store.workspaces.isEmpty && !store.loaded {
                            pickerSkeleton
                        } else {
                            composerWorkspacePicker
                        }
                        Spacer(minLength: 8)
                        composerSendButton
                    }
                    // Menus take the accent tint; pin them monochrome like
                    // the rest of the UI.
                    .tint(.primary)
                }
            }
            .padding(.leading, 16)
            .padding(.trailing, composerFocused ? 12 : 8)
            .padding(.vertical, composerFocused ? 12 : 8)
            .modifier(LiquidGlassPanel(radius: composerFocused ? 22 : 26))
        }
        // High priority so a flick over the text field is the composer's
        // swipe, not the field's own scroll/selection gesture.
        .highPriorityGesture(composerGesture)
        .animation(.easeInOut(duration: 0.2), value: composerFocused)
        .onAppear(perform: applyComposerDefaults)
        .onChange(of: composerKinds.map(\.kind)) { applyComposerDefaults() }
        .onChange(of: store.workspaces.count) { applyComposerDefaults() }
        .onChange(of: composerAgent) { composerModel = composerKind?.defaultModel }
        .padding(.horizontal, 16)
        .padding(.bottom, 6)
    }

    private var composerSendButton: some View {
        Button { Task { await sendComposer() } } label: {
            if composerSending {
                ProgressView()
                    .frame(width: 28, height: 28)
            } else {
                Image(systemName: "arrow.up")
                    .font(.system(size: 13, weight: .semibold))
                    .foregroundStyle(Color(.systemBackground))
                    .frame(width: 28, height: 28)
                    .background(
                        canSendComposer
                            ? Color.primary
                            : Color.secondary.opacity(0.3),
                        in: Circle()
                    )
            }
        }
        .disabled(!canSendComposer || composerSending)
        .accessibilityLabel("Send")
    }

    // MARK: - Composer pickers (transparent text menus, like the sheets)

    /// Selection state for the focused composer — defaults mirror the
    /// sheets (first kind on the workspace's machine, active workspace).
    @State private var composerAgent = ""
    @State private var composerModel: String?
    @State private var composerWorkspaceID: String?

    private var composerWorkspace: Workspace? {
        store.workspaces.first(where: { $0.workspace_id == composerWorkspaceID })
    }

    /// The machine the workspace choice resolves to — runs and kind
    /// catalogs both follow it.
    private var composerMachine: String {
        composerWorkspace?.machine ?? store.machineOrder.first ?? "local"
    }

    private var composerKinds: [HubAgentKind] {
        store.kinds(for: composerMachine)
    }

    private var composerKind: HubAgentKind? {
        composerKinds.first(where: { $0.kind == composerAgent })
    }

    private var composerSpansMachines: Bool {
        Set(store.workspaces.map(\.machine)).count > 1
    }

    private func applyComposerDefaults() {
        if composerAgent.isEmpty, let first = composerKinds.first {
            composerAgent = first.kind
            composerModel = first.defaultModel
        }
        if composerWorkspaceID == nil {
            composerWorkspaceID =
                store.workspaces.first(where: { $0.active == true })?.workspace_id
                ?? store.workspaces.first?.workspace_id
        }
    }

    private var composerAgentPicker: some View {
        Menu {
            ForEach(composerKinds) { kind in
                Button { composerAgent = kind.kind } label: {
                    Label {
                        Text(kind.title)
                    } icon: {
                        if composerAgent == kind.kind {
                            Image(systemName: "checkmark")
                        }
                    }
                }
            }
        } label: {
            composerPickerLabel(
                composerAgent.isEmpty ? "agent" : composerAgent,
                agentKind: composerAgent.isEmpty ? nil : composerAgent
            )
        }
    }

    /// Only shown for kinds the hub reports a model catalog for; older hubs
    /// have no `models` field and this picker simply doesn't appear.
    @ViewBuilder
    private var composerModelPicker: some View {
        if let models = composerKind?.models, !models.isEmpty {
            Menu {
                ForEach(models) { entry in
                    Button { composerModel = entry.name } label: {
                        Label(entry.name,
                              systemImage: composerModel == entry.name ? "checkmark" : "cpu")
                    }
                }
            } label: {
                composerPickerLabel(composerModel ?? "model", systemImage: "cpu")
            }
        }
    }

    private var composerWorkspacePicker: some View {
        Menu {
            ForEach(store.workspaces) { workspace in
                Button {
                    composerWorkspaceID = workspace.workspace_id
                    // The workspace pins the machine the kind catalog comes
                    // from; re-pick the kind if it changed away.
                    if !composerKinds.isEmpty,
                       !composerKinds.contains(where: { $0.kind == composerAgent }),
                       let first = composerKinds.first {
                        composerAgent = first.kind
                        composerModel = first.defaultModel
                    }
                } label: {
                    Label {
                        Text(composerSpansMachines
                             ? "\(workspace.title) · \(store.machineTitle(workspace.machine))"
                             : workspace.title)
                    } icon: {
                        if composerWorkspaceID == workspace.workspace_id {
                            Image(systemName: "checkmark")
                        } else {
                            Image("lucide-server")
                        }
                    }
                }
            }
        } label: {
            composerPickerLabel(composerWorkspace?.title ?? "workspace",
                                asset: "lucide-server")
        }
    }

    private func composerPickerLabel(_ title: String, systemImage: String? = nil,
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
                .foregroundStyle(.tertiary)
        }
        .foregroundStyle(.primary)
    }

    /// The picker's stand-in while catalogs are still in flight — same
    /// monochrome treatment as the list skeletons.
    private var pickerSkeleton: some View {
        SkeletonBar(height: 10)
            .frame(width: 56)
            .padding(.vertical, 4)
    }

    /// Only fires while the composer is focused and tall: up maximizes it
    /// into the sheet, down folds it back to the capsule.
    private var composerGesture: some Gesture {
        DragGesture(minimumDistance: 28)
            .onEnded { value in
                guard composerFocused else { return }
                if value.translation.height < 0 {
                    composerFocused = false
                    showingCompose = true
                } else {
                    composerFocused = false
                }
            }
    }

    private var canSendComposer: Bool {
        !composerSending
            && !composerDraft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
    }

    /// Sends the draft to the pickers' selection — unfocused sends fall
    /// back to the same defaults the pickers apply (first kind on the
    /// workspace's machine, its default model, the active workspace).
    private func sendComposer() async {
        let text = composerDraft.trimmingCharacters(in: .whitespacesAndNewlines)
        guard canSendComposer else { return }
        let workspace = composerWorkspace
            ?? store.workspaces.first(where: { $0.active == true })
            ?? store.workspaces.first
        let machine = workspace?.machine ?? store.machineOrder.first ?? "local"
        guard let kind = composerKind ?? store.kinds(for: machine).first else { return }
        composerSending = true
        composerError = nil
        composerFocused = false
        do {
            let result = try await store.runAgent(
                agent: kind.kind,
                path: workspace?.cwd ?? workspace?.terminal_cwd ?? "~",
                prompt: text,
                workspaceID: workspace?.workspace_id,
                model: composerModel ?? kind.defaultModel,
                machine: machine
            )
            composerDraft = ""
            await store.refresh()
            if let pane = result.pane {
                navPath.append(Agent(
                    pane: pane,
                    agent: kind.kind,
                    name: result.name,
                    agent_session_title: nil,
                    status: "running",
                    workspace_id: workspace?.workspace_id,
                    workspace_name: workspace?.title,
                    cwd: workspace?.cwd,
                    branch: nil,
                    focused: true,
                    machine: machine
                ))
            }
        } catch {
            composerError = error.localizedDescription
        }
        composerSending = false
    }

    // MARK: - Hero header

    /// The site's hero art (hero-factory.webp) bleeding edge to edge above
    /// the workspaces, washed out into the page background like the web
    /// hero's gradient, with a greeting and the inbox line on top.
    private var heroHeader: some View {
        ZStack(alignment: .bottomLeading) {
            Image("hero-factory")
                .resizable()
                .scaledToFill()
                .frame(maxWidth: .infinity)
                .frame(height: 280)
                .clipped()
            // Readability scrim behind the bar items: lightens the top of
            // the art in light mode, darkens it in dark mode, so the
            // adaptive text/icons always sit on their contrasting tone.
            LinearGradient(colors: [heroScrim.opacity(0.55), heroScrim.opacity(0)],
                           startPoint: .top,
                           endPoint: UnitPoint(x: 0.5, y: 0.6))
            // Bottom edge washes out to the same dark tone in both
            // appearances — the dark mode's fade, preferred by design.
            LinearGradient(colors: [Color.black.opacity(0), Color.black],
                           startPoint: UnitPoint(x: 0.5, y: 0.62),
                           endPoint: .bottom)
            VStack(alignment: .leading, spacing: 2) {
                // The wash is dark in both appearances, so the copy stays
                // light rather than following the page's primary color.
                Text(Self.greeting)
                    .font(.title.weight(.semibold))
                    .foregroundStyle(.white)
                Text("Zero inbox for now")
                    .font(.footnote)
                    .foregroundStyle(.white.opacity(0.75))
            }
            .padding(.horizontal, 20)
            .padding(.bottom, 28)
        }
        .padding(.horizontal, -20)
        .padding(.top, -12)
    }

    @Environment(\.colorScheme) private var colorScheme

    /// The hero's top scrim is dark in both appearances — the bar items
    /// (mark, wordmark, bell) draw in their light treatment on it.
    private var heroScrim: Color { .black }

    private static var greeting: String {
        switch Calendar.current.component(.hour, from: .now) {
        case 5..<12: return "Good morning."
        case 12..<17: return "Good afternoon."
        default: return "Good evening."
        }
    }

    // MARK: - Host menu (tap the brand mark)

    private var hostMenu: some View {
        Menu {
            Button("New Task") { showingNewTask = true }
            if store.canRunAgents {
                Button("Open Workspace") { showingOpenWorkspace = true }
            }
            Button("Pairing") { showingPairing = true }
            Button("Settings") { showingSettings = true }
            Button("Disconnect", role: .destructive) {
                store.disconnect()
            }
        } label: {
            HStack(spacing: 8) {
                PilusMark()
                    .frame(width: 36, height: 36)
                Text("pilus")
                    .font(.title2.weight(.semibold))
                    .padding(.trailing, 8)
                    .padding(.bottom, 6)
            }
            // The hero's top is dark in both appearances — the mark and
            // wordmark always draw in their dark-scheme (white) treatment.
            .environment(\.colorScheme, .dark)
        }
        .tint(.primary)
    }

    /// Placeholder bars standing in for workspaces/agents while the first
    /// refresh is still in flight — full-width so the list's shape reads
    /// before any data lands.
    private var skeleton: some View {
        VStack(alignment: .leading, spacing: 0) {
            ForEach(0..<3, id: \.self) { _ in
                SkeletonBar(height: 16)
                    .padding(.top, 18)
                    .padding(.bottom, 6)
                ForEach(0..<2, id: \.self) { _ in
                    HStack(spacing: 12) {
                        Circle()
                            .fill(.secondary.opacity(0.18))
                            .frame(width: 20, height: 20)
                        SkeletonBar(height: 14)
                    }
                    .padding(.vertical, 8)
                }
            }
        }
    }

    // MARK: - Sections

    /// A collapsible section header: title, a hairline rule that stretches
    /// to fill the row, and a chevron that tips sideways when folded.
    private func sectionHeading(_ title: String,
                                expanded: Bool, toggle: @escaping () -> Void) -> some View {
        Button {
            withAnimation(.easeInOut(duration: 0.18)) { toggle() }
        } label: {
            HStack(spacing: 8) {
                Text(title)
                    .font(.headline)
                    .foregroundStyle(.secondary)
                Rectangle()
                    .fill(Color(uiColor: .separator))
                    .frame(height: 0.5)
                Image(systemName: "chevron.down")
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(.tertiary)
                    .rotationEffect(.degrees(expanded ? 0 : -90))
            }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .padding(.top, 18)
        .padding(.bottom, 6)
    }

    private var machinesSection: some View {
        VStack(alignment: .leading, spacing: 0) {
            sectionHeading("Workspaces",
                           expanded: machinesExpanded) {
                machinesExpanded.toggle()
            }
            if machinesExpanded {
                ForEach(Array(machineGroups.enumerated()), id: \.offset) { _, group in
                    if spansMachines {
                        Text(store.machineTitle(group.machine))
                            .font(.subheadline.weight(.medium))
                            .foregroundStyle(.secondary)
                            .padding(.top, 18)
                    }
                    ForEach(group.workspaces, id: \.workspace.id) { node in
                        workspaceGroup(node.workspace)
                        ForEach(node.worktrees) { worktree in
                            worktreeGroup(worktree)
                        }
                    }
                }
            }
        }
    }

    private var agentsSection: some View {
        VStack(alignment: .leading, spacing: 0) {
            sectionHeading("Agents",
                           expanded: agentsExpanded) {
                agentsExpanded.toggle()
            }
            if agentsExpanded {
                ForEach(orphanAgents) { agent in
                    agentRow(agent)
                }
            }
        }
    }

    private var tasksSection: some View {
        VStack(alignment: .leading, spacing: 0) {
            sectionHeading("Tasks",
                           expanded: tasksExpanded) {
                tasksExpanded.toggle()
            }
            if tasksExpanded {
                ForEach(store.tasks, id: \.uiID) { task in
                    taskRow(task)
                }
            }
        }
    }

    // MARK: - Groups

    private func workspaceGroup(_ workspace: Workspace) -> some View {
        let rows = agents(in: workspace)
        return VStack(alignment: .leading, spacing: 0) {
            HStack(alignment: .firstTextBaseline, spacing: 6) {
                Text(workspace.title)
                    .font(.headline)
                Spacer()
            }
            .padding(.top, 18)
            .padding(.bottom, 6)
            if rows.isEmpty {
                // No agents under a workspace still earns a line: its folder.
                HStack(spacing: 10) {
                    Image("lucide-server")
                        .resizable()
                        .renderingMode(.template)
                        .aspectRatio(contentMode: .fit)
                        .frame(width: 16, height: 16)
                        .foregroundStyle(.primary)
                    Text(workspace.cwd ?? workspace.terminal_cwd ?? "")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                        .truncationMode(.middle)
                }
                .padding(.vertical, 6)
            } else {
                ForEach(rows) { agent in
                    agentRow(agent)
                }
            }
        }
    }

    /// A linked worktree hangs off its repo's workspace as an indented
    /// group. No label: the daemon names the workspace after its branch, and
    /// the agents already carry that branch in their subtitle.
    private func worktreeGroup(_ workspace: Workspace) -> some View {
        let rows = agents(in: workspace)
        return VStack(alignment: .leading, spacing: 0) {
            if rows.isEmpty {
                // Nothing hangs under the indent — fall back to the path,
                // like an agentless workspace's folder line.
                Text(workspace.cwd ?? workspace.terminal_cwd ?? "")
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.middle)
                    .padding(.top, 10)
            } else {
                ForEach(rows) { agent in
                    agentRow(agent)
                }
            }
        }
        .padding(.leading, 20)
    }

    /// A task row opens the pane its workspace worker runs in — the task
    /// carries a `tab_id`, `tab.get` answers the pane.
    private func taskRow(_ task: OrchTask) -> some View {
        Button {
            guard openingTaskID == nil else { return }
            openingTaskID = task.uiID
            Task {
                defer { openingTaskID = nil }
                if let agent = try? await store.taskAgent(task) {
                    navPath.append(agent)
                }
            }
        } label: {
            HStack(spacing: 12) {
                TaskIconView(status: task.status, size: 26)
                VStack(alignment: .leading, spacing: 2) {
                    Text(task.title ?? task.id)
                        .font(.body.weight(.medium))
                    if let status = task.status {
                        Text(status)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                }
                Spacer()
                if openingTaskID == task.uiID {
                    ProgressView().controlSize(.small)
                }
            }
            .padding(.vertical, 8)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }

    private func agentRow(_ agent: Agent) -> some View {
        Button {
            navPath.append(agent)
        } label: {
            HStack(spacing: 12) {
                AgentIconView(kind: agent.kind, size: 20)
                VStack(alignment: .leading, spacing: 2) {
                    Text(agent.title)
                        .font(.body.weight(.medium))
                    Text(subtitle(for: agent))
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                        .truncationMode(.middle)
                }
                Spacer(minLength: 8)
                // A working agent turns where the dot sits — the desktop's
                // `style::spinner` convention — so the row says "busy"
                // without waiting on a status push. Both states share one
                // centered frame so switching never shifts the row.
                if agent.status == "working" {
                    ProgressView()
                        .controlSize(.mini)
                        .tint(statusColor(agent.status))
                        .frame(width: 14, height: 14)
                } else {
                    Circle()
                        .fill(statusColor(agent.status))
                        .frame(width: 7, height: 7)
                        .frame(width: 14, height: 14)
                }
            }
            .padding(.vertical, 8)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }

    private func subtitle(for agent: Agent) -> String {
        // [branch] [directory]
        [agent.branch, agent.cwd ?? agent.workspace_name]
            .compactMap { $0 }
            .filter { !$0.isEmpty }
            .joined(separator: " · ")
    }

    /// Monochrome status: a working agent reads as a filled dot, idle as a
    /// mid-tone, everything else as a faint one.
    private func statusColor(_ status: String?) -> Color {
        switch status {
        case "running", "working", "waiting_permission": return .primary
        case "idle", "done": return .secondary
        default: return .secondary.opacity(0.45)
        }
    }
}

/// The real per-kind icon from the desktop app's asset set
/// (`crates/pilus/assets/icons/agents`), bundled as template vectors so the
/// black-and-white scheme tints them.
struct AgentIconView: View {
    let kind: String
    var size: CGFloat = 18

    var body: some View {
        Image(Self.asset(for: kind))
            .resizable()
            .renderingMode(.template)
            .aspectRatio(contentMode: .fit)
            .frame(width: size, height: size)
            .foregroundStyle(.primary)
    }

    static func asset(for kind: String) -> String {
        switch kind {
        case "claude", "claude-code": "agent-claude-code"
        case "pi": "agent-pi"
        case "codex": "agent-codex"
        case "gemini": "agent-gemini"
        case "cursor": "agent-cursor"
        case "opencode": "agent-opencode"
        case "kimi": "agent-kimi"
        case "qwen": "agent-qwen"
        case "antigravity": "agent-antigravity"
        case "kilo-code", "kilocode": "agent-kilo-code"
        case "arc": "agent-arc"
        default: "agent-bot"
        }
    }
}

/// The circle-based task status icons from the desktop asset set
/// (`crates/pilus/assets/icons/task-*.svg`), bundled as template vectors.
struct TaskIconView: View {
    let status: String?
    var size: CGFloat = 18

    var body: some View {
        Image(Self.asset(for: status))
            .resizable()
            .renderingMode(.template)
            .aspectRatio(contentMode: .fit)
            .frame(width: size, height: size)
            .foregroundStyle(.primary)
    }

    static func asset(for status: String?) -> String {
        switch status {
        case "running": "task-running"
        case "failed": "task-failed"
        case "done", "succeeded", "success": "task-done"
        case "blocked": "task-blocked"
        case "claimed": "task-claimed"
        case "merging": "task-merging"
        case "review": "task-review"
        default: "task-queued"
        }
    }
}

/// A skeleton placeholder bar — full width, the gray bars apps draw while
/// real rows are still in flight.
private struct SkeletonBar: View {
    var height: CGFloat = 14

    var body: some View {
        RoundedRectangle(cornerRadius: 4)
            .fill(.secondary.opacity(0.15))
            .frame(maxWidth: .infinity)
            .frame(height: height)
    }
}


