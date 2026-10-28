import OSLog
import SwiftUI

private let log = Logger(subsystem: "dev.usepilus.ios", category: "agent-detail")

struct AgentDetailView: View {
    let agent: Agent

    @Environment(AppStore.self) private var store
    @State private var detail: AgentDetail?
    @State private var content = ""
    /// The agent's own transcript, once its session is found; until then
    /// (or for a kind whose transcripts are not read) the pane is shown.
    @State private var transcript: TranscriptReply?
    @State private var session: SessionSummary?
    /// Until the first session lookup answers, a skeleton stands in rather
    /// than flashing the pane before the transcript replaces it.
    @State private var lookedUp = false
    private var awaitingTranscript: Bool {
        !lookedUp && ["claude", "pi"].contains(agent.kind) && store.hubAllows("hub.sessions", on: agent.machine)
    }
    @State private var refreshes = 0
    /// Set once this screen has asked the hub which session is the agent's;
    /// a remembered match is only shown until then, never trusted.
    @State private var matched = false
    @State private var depth = TranscriptDepth()
    @State private var exhausted = false
    @State private var composerHeight: CGFloat = 0
    @State private var transcriptError: String?
    /// Sent messages shown at the transcript's end until a re-read has them.
    @State private var pending: [TranscriptEntry] = []
    @State private var reply = ""
    // Kept separate so a poll cannot erase a send error before it is read.
    @State private var loadError: String?
    @State private var sendError: String?
    @State private var sending = false
    @State private var showingInfo = false
    @State private var showingDiff = false
    @State private var showingLog = false
    @State private var showingGitHub = false
    /// The shortcut strip starts folded — it shares the composer row's width.
    /// Owned here so the keyboard-down shortcut can resign the field.
    @FocusState private var replyFocused: Bool
    /// Scroll-back pagination: the capture window grows ×4 each time the top
    /// sentinel appears, until the daemon stops reporting `truncated` or the
    /// cap is hit. terminal.backend.capture has no offset cursor — a bigger
    /// window is the only way to reach older history.
    @State private var captureLines = 300
    @State private var truncated = false
    @State private var loadingOlder = false
    /// Set when a widened capture is rejected — the daemon's `capture_lines`
    /// v1 limit is a hard cap (300 today), so deeper history needs a daemon
    /// change; once seen, the sentinel stops offering.
    @State private var olderUnavailable = false
    /// Pins the pane to the bottom only while live-tailing; scrolled up, the
    /// offset is preserved instead of snapping to live on every poll.
    @State private var atBottom = true
    @State private var readingOutput = false
    @State private var outputPending = false
    private static let maxCaptureLines = 16384
    /// The send button's diameter, and the height of one line of the reply.
    private static let sendSize: CGFloat = 30
    private static let lineHeight: CGFloat = 22

    var body: some View {
        GeometryReader { geo in
            paneScroll(geo: geo)
                // The composer floats on glass over the transcript, as the
                // session screen's does, rather than below it.
                .safeAreaInset(edge: .bottom, spacing: 0) { composer }
        }
        .refreshable { await refresh() }
        .navigationTitle(detail?.agent_session_title ?? agent.title)
        .navigationBarTitleDisplayMode(.inline)
        // Status + working directory ride in the nav bar center, under the
        // agent's name — they describe the session, not the content.
        .toolbar {
            // Status + working directory ride in the nav bar center inside a
            // glass capsule — they describe the session, not the content.
            ToolbarItem(placement: .principal) {
                // Status dot + session title in a tappable glass capsule —
                // it opens the same session sheet the ⋯ shows.
                Button { showingInfo = true } label: {
                    HStack(spacing: 6) {
                        if detail?.status != nil {
                            Circle()
                                .fill(.secondary)
                                .frame(width: 6, height: 6)
                        }
                        Text(detail?.agent_session_title ?? agent.title)
                            .font(.subheadline.weight(.semibold))
                            .lineLimit(1)
                    }
                    .foregroundStyle(.primary)
                    // A fixed floor keeps short titles from snapping the
                    // capsule to the absolute center.
                    .frame(minWidth: 150)
                    .padding(.horizontal, 14)
                    .padding(.vertical, 7)
                    .modifier(GlassCapsule())
                }
                .buttonStyle(.plain)
            }
            // Everything about the session lives behind ⋯: the record itself,
            // the Mac's changed files (with notes to hand back to an agent,
            // offered only when the connector lets this device read a diff),
            // and the host's commit history for the workspace it has open.
            ToolbarItem(placement: .topBarTrailing) {
                Menu {
                    Button("Session Info", systemImage: "info.circle") { showingInfo = true }
                    if store.canViewDiff {
                        Button("Changes", systemImage: "plusminus") { showingDiff = true }
                    }
                    if store.supports("git.log", on: agent.machine) {
                        Button("History", systemImage: "clock.arrow.circlepath") { showingLog = true }
                    }
                    // Issues and pull requests come from `gh` on the host, so
                    // they need the connector's `hub.gh.*` and a known checkout.
                    if store.supports("hub.gh.issues", on: agent.machine), agent.cwd?.isEmpty == false {
                        Button("GitHub", systemImage: "arrow.triangle.pull") { showingGitHub = true }
                    }
                } label: {
                    Image(systemName: "ellipsis")
                }
                .tint(.primary)
                .accessibilityLabel("More")
            }
        }
        .sheet(isPresented: $showingInfo) {
            AgentInfoSheet(agent: agent, detail: detail)
        }
        .sheet(isPresented: $showingDiff) {
            DiffReviewSheet(agent: agent)
                .presentationDragIndicator(.hidden)
        }
        .sheet(isPresented: $showingLog) {
            GitLogSheet(agent: agent)
                .presentationDragIndicator(.hidden)
        }
        .sheet(isPresented: $showingGitHub) {
            GitHubSheet(agent: agent)
                .presentationDragIndicator(.hidden)
        }
        // The paired host's watch says when this pane wrote output, so the
        // screen is re-read then. The timer is only a safety net while the
        // watch is up; without it (no hub, or a saved SSH machine, which has no
        // event feed) it is the way the screen stays current.
        .task {
            if session == nil, let known = store.agentSessions[AppStore.sessionKey(agent)],
               known.kind == agent.kind,
               known.cwd == nil || agent.cwd == nil || Self.folder(known.cwd) == Self.folder(agent.cwd) {
                session = known
                transcript = store.cachedTranscript(path: known.path, machine: known.machine)
            }
            await refresh()
            while !Task.isCancelled {
                try? await Task.sleep(for: .seconds(watchDriven ? 20 : 4))
                await refresh()
            }
        }
        .onChange(of: store.outputSignals[agent.id]) {
            Task { await refreshOnOutput() }
        }
    }

    private var composer: some View {
        VStack(spacing: 0) {
            if let shown = sendError ?? loadError {
                Text(shown)
                    .font(.footnote)
                    .foregroundStyle(.red)
                    .padding(.horizontal)
            }

            VStack(spacing: 6) {
                // Bottom-aligned so the send button stays at the corner as the
                // field grows upward.
                HStack(alignment: .bottom, spacing: 8) {
                    TextField("Reply to \(agent.title)", text: $reply, axis: .vertical)
                        .lineLimit(1...5)
                        .textFieldStyle(.plain)
                        .focused($replyFocused)
                        // Agents read this verbatim; autocorrection corrupts
                        // commands and flags.
                        .autocorrectionDisabled()
                        .textInputAutocapitalization(.never)
                        // Centers one line on the send button; more lines grow
                        // the field from there.
                        .padding(.vertical, (Self.sendSize - Self.lineHeight) / 2)
                    if sending {
                        // The send button's footprint, held so the field
                        // does not collapse while the spinner swaps in.
                        ProgressView()
                            .frame(width: Self.sendSize, height: Self.sendSize)
                    } else {
                        sendButton
                    }
                }
                .padding(.leading, 16)
                .padding(.trailing, 6)
                .padding(.vertical, 6)
                // A capsule's radius is half its height, so a tall field
                // balloons into an oval. One fixed radius is a capsule at one
                // line and a rounded rectangle after.
                .modifier(GlassRounded(radius: Self.sendSize / 2 + 6))
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 8)
        }
        .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { composerHeight = $0 }
    }

    /// Output signals arrive for the paired host's panes while its watch is up.
    private var watchDriven: Bool {
        store.watchLive && agent.machine == "local"
    }

    /// Re-read after an output signal, at most one read in flight: signals that
    /// land during a read collapse into one more read afterwards.
    private func refreshOnOutput() async {
        guard !readingOutput else {
            outputPending = true
            return
        }
        readingOutput = true
        defer { readingOutput = false }
        repeat {
            outputPending = false
            await refresh(silent: true)
        } while outputPending && !Task.isCancelled
    }

    /// Tap sends the reply as a prompt. A blocked or not-yet-ready agent falls
    /// back to `agent.keys`; holding still offers terminal input actions.
    private var sendButton: some View {
        Menu {
            Button("Type into terminal and press Enter", systemImage: "return") {
                Task { await send(.submitText(trimmedReply)) }
            }
            .disabled(!store.canSend(.submitText(""), to: agent))
            Button("Type without Enter", systemImage: "keyboard") {
                Task { await send(.typeLiteral(trimmedReply)) }
            }
            .disabled(!store.canSend(.typeLiteral(""), to: agent))
            Button("Paste without Enter", systemImage: "doc.on.clipboard") {
                Task { await send(.pasteText(trimmedReply)) }
            }
            .disabled(!store.canSend(.pasteText(""), to: agent))
        } label: {
            SendArrow(enabled: canSend, size: Self.sendSize)
        } primaryAction: {
            log.info("send tapped")
            Task { await send() }
        }
        .disabled(!canSend)
        .accessibilityLabel("Send")
    }

    private var trimmedReply: String {
        reply.trimmingCharacters(in: .whitespacesAndNewlines)
    }

    private var canSend: Bool {
        !sending && !reply.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
    }

    private var canAnswer: Bool {
        store.supports("agent.keys", on: agent.machine)
    }

    /// iOS 18+ tracks whether the viewport is at the live edge so the bottom
    /// anchor only applies while live-tailing; older releases keep the
    /// always-bottom behavior this view shipped with.
    @ViewBuilder
    private func paneScroll(geo: GeometryProxy) -> some View {
        if #available(iOS 18.0, *) {
            paneScrollContent(geo: geo)
                .defaultScrollAnchor(atBottom ? .bottom : nil)
                .onScrollGeometryChange(for: Bool.self) { geometry in
                    geometry.contentOffset.y + geometry.containerSize.height
                        >= geometry.contentSize.height - 24
                } action: { newValue, _ in atBottom = newValue }
        } else {
            paneScrollContent(geo: geo)
                .defaultScrollAnchor(.bottom)
        }
    }

    private func paneScrollContent(geo: GeometryProxy) -> some View {
        ScrollView {
            VStack(spacing: 0) {
                // Scroll-back sentinel: appearing at the top loads the next
                // ×4 history window; the spinner swaps in while the fetch
                // runs. Only exists when the daemon said more history
                // remains.
                if truncated && !olderUnavailable {
                    Group {
                        if loadingOlder {
                            ProgressView().padding(8)
                        } else {
                            Color.clear.frame(height: 1)
                        }
                    }
                    .frame(maxWidth: .infinity)
                    .onAppear { Task { await loadOlder() } }
                }
                Spacer(minLength: 0)
                if let transcript {
                    if transcript.entries.isEmpty && pending.isEmpty {
                        TranscriptNotice(title: "Nothing said yet")
                    } else {
                        TranscriptList(
                            entries: transcript.entries + pending,
                            earlier: (transcript.earlier ?? false) && depth.canDeepen && !exhausted,
                            loadEarlier: loadEarlierTranscript)
                    }
                } else if let transcriptError {
                    TranscriptNotice(title: "Couldn't read this chat", detail: transcriptError)
                } else if awaitingTranscript {
                    TranscriptSkeleton()
                } else {
                Text(content.isEmpty
                     ? AttributedString("Reading pane")
                     : ANSIRenderer.attributed(content))
                    .font(.system(.caption, design: .monospaced))
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding()
                    .textSelection(.enabled)
                }
            }
            // The pane fills the viewport like a terminal would: short
            // output hugs the bottom edge, long output opens scrolled to
            // the last row and stays pinned there.
            .frame(minHeight: max(0, geo.size.height - composerHeight), alignment: .bottom)
        }
    }

    @discardableResult
    private func refresh(silent: Bool = false) async -> Bool {
        do {
            let (detail, screen) = try await store.agentDetail(agent: agent, lines: captureLines)
            self.detail = detail
            content = screen.text
            truncated = screen.truncated
            loadError = nil
            await refreshTranscript(sessionID: detail?.session, cwd: detail?.cwd)
            return true
        } catch {
            if !silent { loadError = error.localizedDescription }
            return false
        }
    }

    /// Find the agent's session and re-read its newest entries. The match is
    /// re-run when the daemon's session id changes, and every few polls
    /// otherwise, so an agent that starts a new session is followed to it.
    private func refreshTranscript(sessionID: String?, cwd: String?) async {
        refreshes += 1
        let id = sessionID ?? ""
        let moved = !id.isEmpty && session?.session_id != id
        if !matched || session == nil || moved || refreshes % 4 == 0 {
            let title = detail?.agent_session_title ?? agent.agent_session_title
            do {
                let found = try await store.session(
                    for: agent, sessionID: sessionID, cwd: cwd, title: title)
                if found?.path != session?.path {
                    transcript = found.flatMap { store.cachedTranscript(path: $0.path, machine: $0.machine) }
                    pending = []
                    depth = TranscriptDepth()
                    exhausted = false
                }
                session = found
                matched = true
            } catch {}
        }
        guard let session else {
            lookedUp = true
            return
        }
        do {
            let reply = try await store.transcript(
                path: session.path, machine: session.machine, depth: depth)
            // A read that raced a re-match belongs to the old session.
            guard self.session?.path == session.path else { return }
            transcript = reply
            transcriptError = nil
            truncated = false
            let said = reply.entries.suffix(20).filter { $0.type == "user" }
                .compactMap { $0.text?.trimmingCharacters(in: .whitespacesAndNewlines) }
            let stale = Int(Date().timeIntervalSince1970) - 120
            pending.removeAll { said.contains($0.text ?? "") || ($0.at ?? 0) < stale }
        } catch {
            if transcript == nil { transcriptError = error.localizedDescription }
        }
        lookedUp = true
    }

    private func loadEarlierTranscript() async -> Int {
        let had = transcript?.entries.count ?? 0
        depth = depth.deeper()
        await refreshTranscript(sessionID: detail?.session, cwd: detail?.cwd)
        let now = transcript?.entries.count ?? 0
        if now <= had { exhausted = true }
        return now + pending.count
    }

    private static func folder(_ path: String?) -> String {
        let path = path ?? ""
        return path.hasSuffix("/") ? String(path.dropLast()) : path
    }

    /// Scrolled to the top with `truncated` still set: widen the capture
    /// window and refetch — the new older rows land above the current head.
    /// A rejected widen means the daemon's capture_lines cap, so the
    /// sentinel retires instead of erroring on every poll.
    private func loadOlder() async {
        guard !loadingOlder, captureLines < Self.maxCaptureLines else { return }
        loadingOlder = true
        defer { loadingOlder = false }
        let previous = captureLines
        captureLines = min(captureLines * 4, Self.maxCaptureLines)
        if await !refresh(silent: true) {
            captureLines = previous
            olderUnavailable = true
        }
    }

    /// Send through `agent.prompt`, falling back to `agent.keys` for a blocked
    /// or not-ready agent. A held-button selection still uses terminal input.
    /// The text stays in the field on any failure because delivery may be
    /// uncertain after a dropped connection.
    private func send(_ input: AppStore.TerminalInput? = nil) async {
        let text = reply.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty, !sending else {
            log.info("send guarded out (empty=\(text.isEmpty) sending=\(self.sending))")
            return
        }
        sending = true
        defer { sending = false }
        let echo = transcript != nil && input == nil
            ? TranscriptEntry(type: "user", text: text, name: nil, summary: nil,
                              output: nil, failed: nil, at: Int(Date().timeIntervalSince1970))
            : nil
        if let echo {
            pending.append(echo)
            reply = ""
        }
        do {
            if let input {
                try await store.terminalInput(agent: agent, input)
            } else if (detail?.status ?? agent.status) == "blocked", canAnswer {
                try await store.answer(agent: agent, text: text)
            } else {
                do {
                    try await store.sendPrompt(agent: agent, text: text)
                } catch let error as PilusError
                    where error.code == "agent_not_ready" && canAnswer {
                    try await store.answer(agent: agent, text: text)
                }
            }
            log.info("send delivered")
            reply = ""
            sendError = nil
            // Give the pane a beat to echo before re-reading.
            try? await Task.sleep(for: .milliseconds(800))
            await refresh()
        } catch {
            log.error("send failed: \(error.localizedDescription)")
            sendError = error.localizedDescription
            if let echo, let index = pending.lastIndex(of: echo) {
                pending.remove(at: index)
                if reply.isEmpty { reply = text }
            }
            if (error as? PilusError)?.isUnconfirmed == true { await refresh(silent: true) }
        }
    }
}

/// Liquid Glass rounded rectangle where available, material + hairline
/// elsewhere — the reply field's shape, which a capsule cannot hold once the
/// text wraps.
struct GlassRounded: ViewModifier {
    var radius: CGFloat

    func body(content: Content) -> some View {
        if #available(iOS 26.0, *) {
            content.glassEffect(.regular.interactive(), in: .rect(cornerRadius: radius))
        } else {
            content
                .background(.regularMaterial, in: .rect(cornerRadius: radius))
                .overlay(RoundedRectangle(cornerRadius: radius).strokeBorder(.quaternary))
        }
    }
}

/// Liquid Glass where available, material + hairline elsewhere.
private struct GlassCapsule: ViewModifier {
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

/// The ⋯ sheet: the full session record — title, kind, status, cwd — with
/// nothing truncated.
private struct AgentInfoSheet: View {
    let agent: Agent
    let detail: AgentDetail?
    @Environment(AppStore.self) private var store
    @State private var alias = ""
    @State private var savingAlias = false
    @State private var aliasError: String?

    var body: some View {
        NavigationStack {
            List {
                LabeledContent("Title", value: detail?.agent_session_title ?? agent.title)
                LabeledContent("Agent", value: detail?.agent ?? agent.agent ?? agent.kind)
                if let name = detail?.name {
                    LabeledContent("Name", value: name)
                }
                if let status = detail?.status {
                    LabeledContent("Status", value: status)
                }
                LabeledContent("Pane", value: agent.pane)
                if let workspace = agent.workspace_name {
                    LabeledContent("Workspace", value: workspace)
                }
                LabeledContent("Machine", value: agent.machine)
                if let cwd = detail?.cwd ?? agent.cwd {
                    LabeledContent("Directory", value: cwd)
                }
                if let branch = agent.branch {
                    LabeledContent("Branch", value: branch)
                }
                if let session = detail?.session {
                    LabeledContent("Session", value: session)
                }
                if store.supports("pane.rename", on: agent.machine) {
                    aliasSection
                }
            }
            .navigationTitle("Session")
            .navigationBarTitleDisplayMode(.inline)
        }
        .presentationDetents([.medium, .large])
        .presentationDragIndicator(.hidden)
        .onAppear { alias = detail?.name ?? agent.name ?? "" }
    }

    /// The host's operator alias for this pane — how a named agent is found
    /// again from a script or another device. Whatever is typed is folded
    /// into a name the daemon accepts, so there is no rule to explain.
    private var aliasSection: some View {
        Section("Alias") {
            HStack(spacing: 12) {
                TextField("e.g. reviewer", text: $alias)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                    .onChange(of: alias) { _, typed in
                        let folded = AppStore.normalizedAlias(typed)
                        if folded != typed { alias = folded }
                    }
                Button("Save") {
                    Task { await saveAlias() }
                }
                .buttonStyle(.borderedProminent)
                .disabled(savingAlias)
            }
            if let aliasError {
                Text(aliasError).font(.footnote).foregroundStyle(.red)
            }
        }
    }

    private func saveAlias() async {
        savingAlias = true
        defer { savingAlias = false }
        do {
            try await store.renameAgent(agent, to: alias)
            aliasError = nil
        } catch {
            aliasError = error.localizedDescription
        }
    }
}
