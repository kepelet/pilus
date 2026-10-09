import OSLog
import SwiftUI

private let log = Logger(subsystem: "dev.usepilus.ios", category: "agent-detail")

struct AgentDetailView: View {
    let agent: Agent

    @Environment(AppStore.self) private var store
    @State private var detail: AgentDetail?
    @State private var content = ""
    @State private var reply = ""
    // Kept separate so a poll cannot erase a send error before it is read.
    @State private var loadError: String?
    @State private var sendError: String?
    @State private var sending = false
    @State private var showingInfo = false
    @State private var showingDiff = false
    /// The shortcut strip starts folded — it shares the composer row's width.
    @State private var shortcutsExpanded = false
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
    private static let maxCaptureLines = 16384

    var body: some View {
        VStack(spacing: 0) {
            GeometryReader { geo in
                paneScroll(geo: geo)
            }
            .refreshable { await refresh() }

            if let shown = sendError ?? loadError {
                Text(shown)
                    .font(.footnote)
                    .foregroundStyle(.red)
                    .padding(.horizontal)
            }

            VStack(spacing: 6) {
                // The shortcut strip lives above the capsule, hugging the
                // right edge — it grows leftward when opened.
                HStack(spacing: 8) {
                    Spacer()
                    if shortcutsExpanded {
                        shortcutBar
                    }
                    shortcutToggle
                }
                .padding(.horizontal, 2)
                HStack(spacing: 8) {
                    TextField("Reply to \(agent.title)…", text: $reply, axis: .vertical)
                        .lineLimit(1...5)
                        .textFieldStyle(.plain)
                        .focused($replyFocused)
                        // Agents read this verbatim; autocorrection corrupts
                        // commands and flags.
                        .autocorrectionDisabled()
                        .textInputAutocapitalization(.never)
                    if sending {
                        // The send button's footprint, held so the capsule
                        // does not collapse while the spinner swaps in.
                        ProgressView()
                            .frame(width: 29, height: 29)
                    } else {
                        Button {
                            log.info("send tapped")
                            Task { await send() }
                        } label: {
                            Image(systemName: "arrow.up")
                                .font(.system(size: 15, weight: .semibold))
                                .foregroundStyle(Color(.systemBackground))
                                .padding(7)
                                .background(
                                    canSend ? Color.primary : Color.secondary.opacity(0.3),
                                    in: Circle()
                                )
                        }
                        .disabled(!canSend)
                        .accessibilityLabel("Send")
                    }
                }
                .padding(.leading, 14)
                .padding(.trailing, 6)
                .padding(.vertical, 6)
                .modifier(GlassCapsule())
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 8)
        }
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
            // The Mac's changed files, with notes to hand back to an agent.
            // Only offered when the connector lets this device read a diff.
            ToolbarItem(placement: .topBarTrailing) {
                if store.canViewDiff {
                    Button { showingDiff = true } label: {
                        Image(systemName: "plusminus")
                    }
                    .tint(.primary)
                    .accessibilityLabel("Review changes")
                }
            }
            // The full record lives behind ⋯ — title, kind, status, and cwd
            // render unclipped in a sheet.
            ToolbarItem(placement: .topBarTrailing) {
                Button { showingInfo = true } label: {
                    Image(systemName: "ellipsis")
                }
                .tint(.primary)
                .accessibilityLabel("Details")
            }
        }
        .sheet(isPresented: $showingInfo) {
            AgentInfoSheet(agent: agent, detail: detail)
        }
        .sheet(isPresented: $showingDiff) {
            DiffReviewSheet(agent: agent)
        }
        // Poll the pane while the view is up — pane.read is cheap and the
        // screen changes under us.
        .task {
            await refresh()
            while !Task.isCancelled {
                try? await Task.sleep(for: .seconds(4))
                await refresh()
            }
        }
    }

    private var canSend: Bool {
        !sending && !reply.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
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
                Text(content.isEmpty
                     ? AttributedString("Reading pane…")
                     : ANSIRenderer.attributed(content))
                    .font(.system(.caption, design: .monospaced))
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding()
                    .textSelection(.enabled)
            }
            // The pane fills the viewport like a terminal would: short
            // output hugs the bottom edge, long output opens scrolled to
            // the last row and stays pinned there.
            .frame(minHeight: geo.size.height, alignment: .bottom)
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
            return true
        } catch {
            if !silent { loadError = error.localizedDescription }
            return false
        }
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

    private func send() async {
        let text = reply.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty, !sending else {
            log.info("send guarded out (empty=\(text.isEmpty) sending=\(self.sending))")
            return
        }
        sending = true
        defer { sending = false }
        do {
            try await store.sendPrompt(pane: agent.pane, machine: agent.machine, text: text)
            log.info("send delivered")
            reply = ""
            sendError = nil
            // Give the pane a beat to echo before re-reading.
            try? await Task.sleep(for: .milliseconds(800))
            await refresh()
        } catch {
            log.error("send failed: \(error.localizedDescription)")
            sendError = error.localizedDescription
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

// MARK: - Shortcut bar

extension AgentDetailView {
    /// The keyboard icon is the whole control: tap folds the strip in and
    /// out, and closing it — or tapping with the keyboard up — also resigns
    /// the reply field.
    var shortcutToggle: some View {
        Button {
            withAnimation(.easeInOut(duration: 0.15)) {
                shortcutsExpanded.toggle()
            }
            if !shortcutsExpanded {
                replyFocused = false
            }
        } label: {
            Image(systemName: "keyboard")
                .font(.system(size: 13))
                .foregroundStyle(.primary)
                .padding(6)
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityLabel("Keyboard shortcuts")
    }

    /// Named keys the daemon understands (`agent.keys`) plus a local
    /// keyboard-dismiss — the pane has no physical keyboard attached.
    var shortcutBar: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 8) {
                shortcutKey("esc", keys: ["escape"])
                shortcutKey("⌃C", keys: ["ctrl+c"])
                shortcutKey("⌃D", keys: ["ctrl+d"])
                shortcutKey("tab", keys: ["tab"])
                shortcutKey("←", keys: ["left"])
                shortcutKey("↑", keys: ["up"])
                shortcutKey("↓", keys: ["down"])
                shortcutKey("→", keys: ["right"])
                shortcutKey("⏎", keys: ["ENTER"])
            }
            .padding(.vertical, 2)
        }
    }

    private func shortcutKey(_ label: String, keys: [String]) -> some View {
        Button {
            Task { try? await store.sendKeys(pane: agent.pane, machine: agent.machine, keys: keys) }
        } label: {
            Text(label)
                .font(.system(size: 11, weight: .medium, design: .monospaced))
                .foregroundStyle(.primary)
                .padding(.horizontal, 10)
                .padding(.vertical, 6)
                .background(.secondary.opacity(0.12), in: .rect(cornerRadius: 8))
        }
        .buttonStyle(.plain)
    }
}

/// The ⋯ sheet: the full session record — title, kind, status, cwd — with
/// nothing truncated.
private struct AgentInfoSheet: View {
    let agent: Agent
    let detail: AgentDetail?
    @Environment(\.dismiss) private var dismiss

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
            }
            .navigationTitle("Session")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .topBarTrailing) {
                    Button("Done") { dismiss() }
                }
            }
        }
        .presentationDetents([.medium, .large])
    }
}
