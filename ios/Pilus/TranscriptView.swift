import SwiftUI

/// A session read from the agent's own transcript: what was asked, what was
/// said, and each tool call folded to one line. Re-read on a slow beat — it
/// is never streamed.
struct TranscriptList: View {
    let entries: [TranscriptEntry]
    var earlier = false
    /// Reads further back; called when the top of the list comes into view
    /// while older entries remain.
    /// Returns how many entries there are once it has read.
    var loadEarlier: (() async -> Int)?

    @State private var loading = false

    var body: some View {
        ScrollViewReader { proxy in
        LazyVStack(alignment: .leading, spacing: 14) {
            if earlier, let loadEarlier {
                ProgressView()
                    .frame(maxWidth: .infinity)
                    .onAppear {
                        guard !loading else { return }
                        loading = true
                        let first = entries.count
                        Task {
                            let now = await loadEarlier()
                            // Keep the entry that was on top in place rather
                            // than jumping to the oldest one just read.
                            if now > first { proxy.scrollTo(now - first, anchor: .top) }
                            loading = false
                        }
                    }
            }
            ForEach(Array(entries.enumerated()), id: \.offset) { _, entry in
                TranscriptRow(entry: entry)
            }
        }
        .padding()
        .frame(maxWidth: .infinity, alignment: .leading)
        }
    }
}

private struct TranscriptRow: View {
    let entry: TranscriptEntry
    @State private var expanded = false

    var body: some View {
        switch entry.type {
        case "user":
            HStack {
                Spacer(minLength: 40)
                Text(markdown(entry.text ?? ""))
                    .padding(.horizontal, 12)
                    .padding(.vertical, 8)
                    .background(.secondary.opacity(0.15), in: .rect(cornerRadius: 14))
                    .textSelection(.enabled)
            }
        case "assistant":
            Text(markdown(entry.text ?? ""))
                .frame(maxWidth: .infinity, alignment: .leading)
                .textSelection(.enabled)
        default:
            tool
        }
    }

    private var tool: some View {
        VStack(alignment: .leading, spacing: 4) {
            Button {
                guard hasOutput else { return }
                var plain = Transaction()
                plain.disablesAnimations = true
                withTransaction(plain) { expanded.toggle() }
            } label: {
                VStack(alignment: .leading, spacing: 2) {
                    Text(entry.name ?? "tool")
                        .font(.caption.weight(.semibold))
                        .foregroundStyle(entry.failed == true ? .red : .primary)
                    if let summary = entry.summary, !summary.isEmpty {
                        Text(summary)
                            .font(.system(.caption, design: .monospaced))
                            .foregroundStyle(.secondary)
                            .lineLimit(expanded ? nil : 1)
                            .truncationMode(.middle)
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            if expanded, let output = entry.output {
                Text(output)
                    .font(.system(.caption2, design: .monospaced))
                    .foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(8)
                    .background(.secondary.opacity(0.08), in: .rect(cornerRadius: 8))
                    .textSelection(.enabled)
            }
        }
        .animation(nil, value: expanded)
    }

    private var hasOutput: Bool { !(entry.output ?? "").isEmpty }

    private func markdown(_ text: String) -> AttributedString {
        (try? AttributedString(
            markdown: text,
            options: .init(interpretedSyntax: .inlineOnlyPreservingWhitespace)))
            ?? AttributedString(text)
    }
}

/// A recent session from Home, opened on its transcript.
struct SessionTranscriptView: View {
    let session: SessionSummary
    /// Opens the agent a Continue or Fork started.
    var onStarted: (Agent) -> Void = { _ in }

    @Environment(AppStore.self) private var store
    @State private var reply: TranscriptReply?
    @State private var loadError: String?
    @State private var prompt = ""
    @State private var starting = false
    @State private var startError: String?
    @State private var confirmingFork = false
    @State private var depth = TranscriptDepth()
    @State private var exhausted = false

    var body: some View {
        ScrollView {
            if let reply {
                if reply.entries.isEmpty {
                    TranscriptNotice(title: "Nothing said yet")
                } else {
                    TranscriptList(
                        entries: reply.entries,
                        earlier: (reply.earlier ?? false) && depth.canDeepen && !exhausted,
                        loadEarlier: loadEarlier)
                }
            } else if let loadError {
                TranscriptNotice(title: "Couldn't read this chat", detail: loadError)
            } else {
                TranscriptSkeleton()
            }
        }
        .defaultScrollAnchor(.bottom)
        .safeAreaInset(edge: .bottom) {
            if store.canReopen(session) { composer }
        }
        .toolbar {
            if store.canReopen(session) {
                ToolbarItem(placement: .topBarTrailing) {
                    Button("Fork", systemImage: "arrow.branch") { confirmingFork = true }
                        .disabled(starting)
                        .confirmationDialog(
                            "Fork this session?", isPresented: $confirmingFork,
                            titleVisibility: .visible
                        ) {
                            Button("Fork") { start(fork: true) }
                            Button("Cancel", role: .cancel) {}
                        } message: {
                            Text("A new session starts with this one's history. This one stays as it is.")
                        }
                }
            }
        }
        .refreshable { await load() }
        .navigationTitle(session.title)
        .navigationBarTitleDisplayMode(.inline)
        .task {
            if reply == nil {
                reply = store.cachedTranscript(path: session.path, machine: session.machine)
            }
            while !Task.isCancelled {
                await load()
                try? await Task.sleep(for: .seconds(15))
            }
        }
    }

    /// Typing continues the session where it left off; the typed text, if
    /// any, is its next prompt.
    private var composer: some View {
        VStack(spacing: 4) {
            if let startError {
                Text(startError)
                    .font(.footnote)
                    .foregroundStyle(.red)
                    .padding(.horizontal)
            }
            HStack(alignment: .bottom, spacing: 8) {
                TextField("Continue this session", text: $prompt, axis: .vertical)
                    .lineLimit(1...5)
                    .lineSpacing(AgentDetailView.lineSpacing)
                    .textFieldStyle(.plain)
                    // Prompts reach the agent verbatim.
                    .autocorrectionDisabled()
                    .textInputAutocapitalization(.never)
                    .padding(.vertical, 4)
                if starting {
                    ProgressView()
                        .frame(width: 30, height: 30)
                } else {
                    Button { start(fork: false) } label: {
                        SendArrow(enabled: true, size: 30)
                    }
                    .accessibilityLabel("Continue")
                }
            }
            .padding(.leading, 16)
            .padding(.trailing, 6)
            .padding(.vertical, 6)
            .modifier(GlassRounded(radius: 21))
            .padding(.horizontal, 12)
            .padding(.bottom, 8)
        }
    }

    private func start(fork: Bool) {
        guard !starting else { return }
        Task {
            starting = true
            startError = nil
            defer { starting = false }
            do {
                let text = prompt.trimmingCharacters(in: .whitespacesAndNewlines)
                if let agent = try await store.reopen(session, fork: fork, prompt: text) {
                    prompt = ""
                    onStarted(agent)
                }
            } catch {
                startError = error.localizedDescription
            }
        }
    }

    private func load() async {
        do {
            reply = try await store.transcript(path: session.path, machine: session.machine, depth: depth)
            loadError = nil
        } catch {
            if reply == nil { loadError = error.localizedDescription }
        }
    }

    private func loadEarlier() async -> Int {
        let had = reply?.entries.count ?? 0
        depth = depth.deeper()
        await load()
        let now = reply?.entries.count ?? 0
        // An older pilus-agent caps the entries it sends, so a read that
        // brings nothing new ends the reaching back.
        if now <= had { exhausted = true }
        return now
    }
}

/// Gray bubbles and lines standing in for a transcript still being read.
struct TranscriptSkeleton: View {
    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            ForEach(0..<3, id: \.self) { _ in
                HStack {
                    Spacer(minLength: 80)
                    RoundedRectangle(cornerRadius: 14)
                        .fill(.secondary.opacity(0.14))
                        .frame(width: 180, height: 34)
                }
                ForEach([1.0, 0.9, 0.6], id: \.self) { fraction in
                    GeometryReader { geo in
                        RoundedRectangle(cornerRadius: 4)
                            .fill(.secondary.opacity(0.14))
                            .frame(width: geo.size.width * fraction, height: 12)
                    }
                    .frame(height: 12)
                }
            }
        }
        .padding(16)
        .frame(maxWidth: .infinity, alignment: .leading)
        .redacted(reason: .placeholder)
    }
}

/// A line in place of a transcript that is empty or could not be read.
struct TranscriptNotice: View {
    let title: String
    var detail: String?

    var body: some View {
        VStack(spacing: 6) {
            Text(title)
                .font(.headline)
                .foregroundStyle(.secondary)
            if let detail {
                Text(detail)
                    .font(.footnote)
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
                    .textSelection(.enabled)
            }
        }
        .frame(maxWidth: .infinity)
        .padding(.horizontal, 24)
        .padding(.top, 40)
    }
}
