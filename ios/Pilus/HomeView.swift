import SwiftUI

/// The single screen: host header, then the live agents on every machine,
/// then the newest agent sessions — both open on their transcript.
/// Deliberately not a `List` — no separators, disclosure chevrons, or
/// section chrome; rows are plain text on the system background and the
/// only accent is monochrome.
struct HomeView: View {
    @Environment(AppStore.self) private var store
    @State private var navPath = NavigationPath()
    /// The draft lives here so closing the compose sheet never throws
    /// text away.
    @State private var composerDraft = ""
    @State private var showingOpenWorkspace = false
    @State private var showingPairing = false
    @State private var showingSettings = false
    @State private var showingCompose = false
    /// Focus opens the composer; this state stays expanded when the keyboard
    /// dismisses so picker menus remain available.
    @State private var composerExpanded = false
    @FocusState private var composerFocused: Bool
    @State private var composerSending = false
    @State private var composerError: String?

    /// In-stack destinations that are not an `Agent`.
    private enum HomeRoute: Hashable {
        case search
    }
    @AppStorage("home.agents.expanded") private var agentsExpanded = true
    @AppStorage("home.sessions.expanded") private var sessionsExpanded = true

    var body: some View {
        NavigationStack(path: $navPath) {
            topContent
            // The hero bleeds to the screen's top edge, behind a bar that
            // draws no material of its own — logo and bell sit bare on it.
            .ignoresSafeArea(.container, edges: .top)
            .toolbarBackground(.hidden, for: .navigationBar)
            .refreshable {
                await store.refresh()
                await store.loadSessions()
            }
            // Sessions have no event feed; a slow re-read keeps the list
            // fresh without streaming anything.
            .task(id: store.loaded) {
                guard store.loaded else { return }
                while !Task.isCancelled {
                    await store.loadSessions()
                    try? await Task.sleep(for: .seconds(30))
                }
            }
            // A tap anywhere outside the composer resigns it — rows and
            // section headers keep their own handlers.
            .onTapGesture { collapseComposer() }
            // Pushing a page (agent, notifications) drops the keyboard.
            .onChange(of: navPath.count) { collapseComposer() }
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
                        if store.canReadSessions {
                            glassButton("magnifyingglass", label: "Search Sessions") {
                                navPath.append(HomeRoute.search)
                            }
                        }
                    }
                    .sharedBackgroundVisibility(.hidden)
                } else {
                    ToolbarItemGroup(placement: .topBarTrailing) {
                        if store.canReadSessions {
                            Button { navPath.append(HomeRoute.search) } label: {
                                Image(systemName: "magnifyingglass")
                            }
                            .accessibilityLabel("Search Sessions")
                        }
                    }
                }
            }
            .navigationDestination(for: Agent.self) { agent in
                AgentDetailView(agent: agent)
            }
            .navigationDestination(for: SessionSummary.self) { session in
                SessionTranscriptView(session: session) { agent in
                    navPath.removeLast()
                    navPath.append(agent)
                }
            }
            .navigationDestination(for: HomeRoute.self) { route in
                switch route {
                case .search: SessionSearchView()
                }
            }
            .sheet(isPresented: $showingOpenWorkspace) {
                OpenWorkspaceView { path, machine in
                    // The composer follows the workspace just opened.
                    composerWorkspaceID = store.workspaces.first {
                        $0.machine == machine && ($0.cwd == path || $0.terminal_cwd == path)
                    }?.workspace_id ?? composerWorkspaceID
                }
                    .presentationDragIndicator(.hidden)
            }
            .sheet(isPresented: $showingPairing) {
                OnboardingView(onPaired: { showingPairing = false })
                    .presentationDragIndicator(.hidden)
            }
            .sheet(isPresented: $showingSettings) {
                SettingsView()
                    .presentationDragIndicator(.hidden)
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
                if store.agents.isEmpty && !store.loaded {
                    skeleton
                } else {
                    // An empty list shows no section at all — no header,
                    // no hairline.
                    if !store.agents.isEmpty {
                        agentsSection
                    }
                    if !store.recentSessions.isEmpty || (store.canReadSessions && !store.sessionsLoaded) {
                        sessionsSection
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
            VStack(alignment: .leading, spacing: composerExpanded ? 10 : 0) {
                HStack(alignment: composerExpanded ? .top : .center, spacing: 8) {
                    TextField("Plan, ask, build", text: $composerDraft, axis: .vertical)
                        .lineLimit(composerExpanded ? 2...6 : 1...1)
                        .textFieldStyle(.plain)
                        .focused($composerFocused)
                        // Prompts reach the agent verbatim.
                        .autocorrectionDisabled()
                        .textInputAutocapitalization(.never)
                    if !composerExpanded {
                        composerSendButton
                    }
                }
                if composerExpanded {
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
            .padding(.trailing, composerExpanded ? 12 : 8)
            .padding(.vertical, composerExpanded ? 12 : 8)
            .modifier(LiquidGlassPanel(radius: composerExpanded ? 22 : 26))
        }
        // High priority so a flick over the text field is the composer's
        // swipe, not the field's own scroll/selection gesture.
        .contentShape(Rectangle())
        .onTapGesture { composerFocused = true }
        .highPriorityGesture(composerGesture)
        .animation(.easeInOut(duration: 0.2), value: composerExpanded)
        .onAppear(perform: applyComposerDefaults)
        .onChange(of: composerFocused) { _, focused in
            if focused {
                composerExpanded = true
            }
        }
        .onChange(of: composerKinds.map(\.kind)) { applyComposerDefaults() }
        .onChange(of: store.workspaces.count) { applyComposerDefaults() }
        .onChange(of: composerAgent) { composerModel = composerKind?.defaultModel }
        .padding(.horizontal, 16)
        // At rest it sits down into the home indicator's margin; over the
        // keyboard it keeps a gap.
        .padding(.bottom, composerFocused ? 6 : -12)
    }

    /// A toolbar button on a Liquid Glass circle, as the composer's, while
    /// the wordmark stays bare over the hero.
    @available(iOS 26.0, *)
    private func glassButton(_ symbol: String, label: String,
                             action: @escaping () -> Void) -> some View {
        Button(action: action) {
            Image(systemName: symbol)
                .frame(width: 20, height: 20)
                .padding(8)
                .glassEffect(.regular.interactive(), in: .circle)
        }
        .buttonStyle(.plain)
        .environment(\.colorScheme, .dark)
        .accessibilityLabel(label)
    }

    private var composerSendButton: some View {
        Button { Task { await sendComposer() } } label: {
            if composerSending {
                ProgressView()
                    .frame(width: 28, height: 28)
            } else {
                SendArrow(enabled: canSendComposer, size: 28)
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
                        Label {
                            Text(entry.name)
                        } icon: {
                            if composerModel == entry.name {
                                Image(systemName: "checkmark")
                            }
                        }
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
                        }
                    }
                }
            }
            if store.canRunAgents {
                Divider()
                Button("Open Workspace…") { showingOpenWorkspace = true }
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
                guard composerExpanded else { return }
                if value.translation.height < 0 {
                    collapseComposer()
                    showingCompose = true
                } else {
                    collapseComposer()
                }
            }
    }

    private var canSendComposer: Bool {
        !composerSending && composerKind != nil
    }

    private func collapseComposer() {
        composerExpanded = false
        composerFocused = false
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
        collapseComposer()
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
                Text(agentSummary)
                    .font(.footnote)
                    .foregroundStyle(.white.opacity(0.75))
                if let overview = overviewSummary {
                    Text(overview)
                        .font(.footnote)
                        .foregroundStyle(.white.opacity(0.55))
                }
            }
            .padding(.horizontal, 20)
            .padding(.bottom, 28)
        }
        .padding(.horizontal, -20)
        .padding(.top, -12)
    }

    @Environment(\.colorScheme) private var colorScheme

    /// What the live agents are doing, busiest first.
    private var agentSummary: String {
        let statuses = store.agents.map(\.status)
        let working = statuses.filter { $0 == "working" || $0 == "running" }.count
        let waiting = statuses.filter { $0 == "blocked" || $0 == "waiting_permission" }.count
        let idle = statuses.count - working - waiting
        var parts: [String] = []
        if working > 0 { parts.append("\(working) working") }
        if waiting > 0 { parts.append("\(waiting) waiting for you") }
        if idle > 0 { parts.append("\(idle) idle") }
        return parts.isEmpty ? "No agents running" : parts.joined(separator: ", ")
    }

    /// What else is going on beyond the agents' states: work waiting on the
    /// board, sessions touched today, and how far the host reaches. Nil when
    /// there is nothing to say, so a fresh pairing shows no empty line.
    private var overviewSummary: String? {
        var parts: [String] = []
        let open = store.tasks.filter {
            !["done", "succeeded", "success", "failed"].contains($0.status ?? "")
        }
        let review = open.filter { $0.status == "review" }.count
        if review > 0 { parts.append("\(review) to review") }
        if !open.isEmpty { parts.append(Self.count(open.count, "task") + " open") }
        // Home lists the newest 20, so a full day of them reads "20+".
        let start = Calendar.current.startOfDay(for: .now).timeIntervalSince1970
        let today = store.sessions.filter { Double($0.updated ?? 0) >= start }.count
        if today > 0 {
            parts.append(Self.count(today, "session") + (today >= 20 ? "+" : "") + " today")
        }
        let machines = store.machineOrder.count
        if machines > 1 { parts.append("\(machines) machines") }
        return parts.isEmpty ? nil : parts.joined(separator: " · ")
    }

    private static func count(_ n: Int, _ noun: String) -> String {
        "\(n) \(noun)\(n == 1 ? "" : "s")"
    }

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

    private var agentsSection: some View {
        VStack(alignment: .leading, spacing: 0) {
            sectionHeading("Active Agents",
                           expanded: agentsExpanded) {
                agentsExpanded.toggle()
            }
            if agentsExpanded {
                ForEach(store.agents) { agent in
                    agentRow(agent)
                }
            }
        }
    }

    private var sessionsSection: some View {
        VStack(alignment: .leading, spacing: 0) {
            sectionHeading("Recent Sessions",
                           expanded: sessionsExpanded) {
                sessionsExpanded.toggle()
            }
            if sessionsExpanded {
                if store.sessions.isEmpty && !store.sessionsLoaded {
                    ForEach(0..<4, id: \.self) { _ in
                        HStack(spacing: 12) {
                            Circle()
                                .fill(.secondary.opacity(0.18))
                                .frame(width: 20, height: 20)
                            VStack(alignment: .leading, spacing: 6) {
                                SkeletonBar(height: 14)
                                SkeletonBar(height: 10)
                                    .frame(width: 140)
                            }
                        }
                        .padding(.vertical, 8)
                    }
                }
                ForEach(store.recentSessions) { session in
                    sessionRow(session)
                }
            }
        }
    }

    private func sessionRow(_ session: SessionSummary) -> some View {
        Button {
            navPath.append(session)
        } label: {
            SessionRowLabel(session: session)
                .padding(.vertical, 8)
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .contextMenu {
            if store.canReopen(session) {
                Button("Continue", systemImage: "play") { reopen(session, fork: false) }
                Button("Fork", systemImage: "arrow.branch") { reopen(session, fork: true) }
            }
        }
    }

    private func reopen(_ session: SessionSummary, fork: Bool) {
        Task {
            do {
                if let agent = try await store.reopen(session, fork: fork) {
                    navPath.append(agent)
                }
            } catch {
                composerError = error.localizedDescription
            }
        }
    }

    private func agentRow(_ agent: Agent) -> some View {
        Button {
            navPath.append(agent)
        } label: {
            HStack(spacing: 12) {
                AgentTile(kind: agent.kind)
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
                switch agent.status {
                case "working":
                    ProgressView()
                        .controlSize(.mini)
                        .tint(statusColor(agent.status))
                        .frame(width: 14, height: 14)
                case "idle", nil:
                    EmptyView()
                default:
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

    /// Monochrome status but for an agent waiting on the user, which is
    /// orange: working reads as a filled dot, done as a mid-tone, everything
    /// else as a faint one. Idle draws nothing.
    private func statusColor(_ status: String?) -> Color {
        switch status {
        case "blocked", "waiting_permission": return .orange
        case "running", "working": return .primary
        case "done": return .secondary
        default: return .secondary.opacity(0.45)
        }
    }
}

/// An agent's icon on a square card in its kind's colors, as the desktop's
/// tab glyph tiles: Claude's orange with a cream mark, any other agent the
/// terminal's near-black with a green mark.
struct AgentTile: View {
    let kind: String
    var size: CGFloat = 30

    private var colors: (face: Color, glyph: Color) {
        switch kind {
        case "claude", "claude-code":
            (Color(red: 0xd9 / 255, green: 0x77 / 255, blue: 0x57 / 255),
             Color(red: 0xfa / 255, green: 0xf9 / 255, blue: 0xf5 / 255))
        default:
            (Color(red: 0x0f / 255, green: 0x12 / 255, blue: 0x10 / 255),
             Color(red: 0x4a / 255, green: 0xde / 255, blue: 0x80 / 255))
        }
    }

    var body: some View {
        let colors = colors
        RoundedRectangle(cornerRadius: size * 0.27, style: .continuous)
            .fill(colors.face)
            .overlay {
                RoundedRectangle(cornerRadius: size * 0.27, style: .continuous)
                    .strokeBorder(colors.glyph.opacity(0.35), lineWidth: 1)
            }
            .overlay {
                Image(AgentIconView.asset(for: kind))
                    .resizable()
                    .renderingMode(.template)
                    .aspectRatio(contentMode: .fit)
                    .frame(width: size * 0.56, height: size * 0.56)
                    .foregroundStyle(colors.glyph)
            }
            .frame(width: size, height: size)
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

/// A session's row: its agent, title, and when and where it ran.
struct SessionRowLabel: View {
    let session: SessionSummary
    @Environment(AppStore.self) private var store

    var body: some View {
        HStack(spacing: 12) {
            AgentTile(kind: session.kind)
            VStack(alignment: .leading, spacing: 2) {
                Text(session.title)
                    .font(.body.weight(.medium))
                    .lineLimit(1)
                Text(subtitle)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.middle)
            }
            Spacer(minLength: 8)
        }
    }

    private var subtitle: String {
        var parts: [String] = []
        if let updated = session.updated, updated > 0 {
            parts.append(Date(timeIntervalSince1970: TimeInterval(updated))
                .formatted(.relative(presentation: .named)))
        }
        if let cwd = session.cwd, !cwd.isEmpty {
            parts.append((cwd as NSString).lastPathComponent)
        }
        if store.machineOrder.count > 1 {
            parts.append(store.machineTitle(session.machine))
        }
        return parts.joined(separator: " · ")
    }
}

/// A session row's stand-in while a search is still out.
private struct SessionRowSkeleton: View {
    var body: some View {
        HStack(spacing: 12) {
            RoundedRectangle(cornerRadius: 8, style: .continuous)
                .fill(.secondary.opacity(0.15))
                .frame(width: 30, height: 30)
            VStack(alignment: .leading, spacing: 6) {
                SkeletonBar(height: 14)
                SkeletonBar(height: 10)
                    .frame(width: 140)
            }
        }
        .padding(.vertical, 8)
    }
}

/// Sessions the hosts keep, but the live agents', searched on each host:
/// each word must appear in the title, folder, branch or agent.
struct SessionSearchView: View {
    @Environment(AppStore.self) private var store
    @State private var query = ""
    @State private var found: [SessionSummary]?

    var body: some View {
        ScrollView {
            LazyVStack(spacing: 0) {
                ForEach(found ?? []) { session in
                    NavigationLink(value: session) {
                        SessionRowLabel(session: session)
                            .padding(.horizontal, 20)
                            .padding(.vertical, 8)
                            .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                }
            }
        }
        .overlay {
            if found == nil {
                // Rows the shape of the real ones, rather than a spinner.
                VStack(spacing: 0) {
                    ForEach(0..<8, id: \.self) { _ in SessionRowSkeleton() }
                    Spacer(minLength: 0)
                }
                .padding(.horizontal, 20)
                .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
                .background(Color(uiColor: .systemBackground))
            } else if found?.isEmpty == true {
                if query.isEmpty {
                    ContentUnavailableView("No sessions", systemImage: "text.bubble")
                } else {
                    ContentUnavailableView.search(text: query)
                }
            }
        }
        .searchable(text: $query, placement: .navigationBarDrawer(displayMode: .always),
                    prompt: "Search sessions")
        .navigationTitle("Sessions")
        .navigationBarTitleDisplayMode(.inline)
        .task(id: query) {
            let query = query.trimmingCharacters(in: .whitespaces)
            // What this query found last time shows at once; a recent one is
            // not asked again, an older one is while it stays on screen.
            let cached = store.cachedSearch(query)
            if let cached {
                found = cached.rows
                if cached.fresh { return }
            }
            if !query.isEmpty, cached == nil {
                try? await Task.sleep(for: .milliseconds(300))
                if Task.isCancelled { return }
            }
            let rows = await store.searchSessions(query)
            if !Task.isCancelled { found = rows }
        }
    }
}
