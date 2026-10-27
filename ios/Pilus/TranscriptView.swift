import SwiftUI

/// A session read from the agent's own transcript: what was asked, what was
/// said, and each tool call folded to one line. Re-read on a slow beat — it
/// is never streamed.
struct TranscriptList: View {
    let entries: [TranscriptEntry]
    var earlier = false

    var body: some View {
        LazyVStack(alignment: .leading, spacing: 14) {
            if earlier {
                Text("Earlier messages are on the desktop")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity)
            }
            ForEach(Array(entries.enumerated()), id: \.offset) { _, entry in
                TranscriptRow(entry: entry)
            }
        }
        .padding()
        .frame(maxWidth: .infinity, alignment: .leading)
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
                if hasOutput { expanded.toggle() }
            } label: {
                HStack(alignment: .firstTextBaseline, spacing: 6) {
                    Image(systemName: entry.failed == true ? "xmark.circle" : "wrench.and.screwdriver")
                        .font(.caption2)
                        .foregroundStyle(entry.failed == true ? .red : .secondary)
                    Text(entry.name ?? "tool")
                        .font(.caption.weight(.semibold))
                    Text(entry.summary ?? "")
                        .font(.system(.caption, design: .monospaced))
                        .foregroundStyle(.secondary)
                        .lineLimit(expanded ? nil : 1)
                        .truncationMode(.middle)
                    Spacer(minLength: 0)
                }
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
    @State private var choosing = false

    var body: some View {
        ScrollView {
            if let reply {
                if reply.entries.isEmpty {
                    Text("Nothing said yet")
                        .foregroundStyle(.secondary)
                        .frame(maxWidth: .infinity)
                        .padding(.top, 40)
                } else {
                    TranscriptList(entries: reply.entries, earlier: reply.earlier ?? false)
                }
            } else if let loadError {
                Text(loadError)
                    .font(.footnote)
                    .foregroundStyle(.red)
                    .padding()
            } else {
                TranscriptSkeleton()
            }
        }
        .defaultScrollAnchor(.bottom)
        .safeAreaInset(edge: .bottom) {
            if store.canReopen(session) {
                Button { choosing = true } label: {
                    HStack(spacing: 8) {
                        Text("Continue or fork this session")
                            .foregroundStyle(.secondary)
                            .frame(maxWidth: .infinity, alignment: .leading)
                        Image(systemName: "arrow.up")
                            .font(.system(size: 14, weight: .semibold))
                            .foregroundStyle(.white)
                            .frame(width: 30, height: 30)
                            .background(Color.accentColor, in: .circle)
                    }
                    .padding(.leading, 16)
                    .padding(.trailing, 6)
                    .padding(.vertical, 6)
                    .contentShape(.rect)
                    .modifier(GlassRounded(radius: 21))
                }
                .buttonStyle(.plain)
                .padding(.horizontal, 12)
                .padding(.bottom, 8)
            }
        }
        .sheet(isPresented: $choosing) {
            ReopenSheet(session: session) { agent in
                choosing = false
                onStarted(agent)
            }
            .presentationDetents([.height(280)])
        }
        .refreshable { await load() }
        .navigationTitle(session.title)
        .navigationBarTitleDisplayMode(.inline)
        .task {
            while !Task.isCancelled {
                await load()
                try? await Task.sleep(for: .seconds(15))
            }
        }
    }

    private func load() async {
        do {
            reply = try await store.transcript(path: session.path, machine: session.machine)
            loadError = nil
        } catch {
            if reply == nil { loadError = error.localizedDescription }
        }
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

/// Continue the session where it left off, or fork it into a new one.
struct ReopenSheet: View {
    let session: SessionSummary
    var onStarted: (Agent) -> Void

    @Environment(AppStore.self) private var store
    @Environment(\.dismiss) private var dismiss
    @State private var starting: Bool?
    @State private var startError: String?

    var body: some View {
        NavigationStack {
            VStack(spacing: 20) {
                choice(fork: false, title: "Continue", detail: "Pick up in this session",
                       systemImage: "play.fill")
                choice(fork: true, title: "Fork", detail: "Start a new session from here",
                       systemImage: "arrow.branch")
                if let startError {
                    Text(startError)
                        .font(.footnote)
                        .foregroundStyle(.red)
                }
                Spacer(minLength: 0)
            }
            .padding(20)
            .navigationTitle(session.title)
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel", systemImage: "xmark") { dismiss() }
                }
            }
        }
    }

    @ViewBuilder
    private func choice(fork: Bool, title: String, detail: String, systemImage: String) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(detail)
                .font(.footnote)
                .foregroundStyle(.secondary)
            let button = Button {
                Task {
                    starting = fork
                    startError = nil
                    do {
                        if let agent = try await store.reopen(session, fork: fork) {
                            onStarted(agent)
                        } else {
                            dismiss()
                        }
                    } catch {
                        startError = error.localizedDescription
                    }
                    starting = nil
                }
            } label: {
                HStack {
                    if starting == fork {
                        ProgressView()
                    } else {
                        Image(systemName: systemImage)
                    }
                    Text(title)
                }
                .frame(maxWidth: .infinity)
            }
            .controlSize(.large)
            .disabled(starting != nil)
            if fork {
                button.buttonStyle(.bordered)
            } else {
                button.buttonStyle(.borderedProminent)
            }
        }
    }
}
