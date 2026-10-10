import Foundation

// The daemon's `diff.*` replies, as the hub relays them. Decoding is lenient
// on purpose: an unknown layer, kind or state never fails a whole list, it
// just reads as the nearest known value, so a newer Luvus still renders.

/// Where a changed file sits in Git. A path changed in both the index and the
/// working tree appears once per layer.
enum DiffLayer: String, CaseIterable, Hashable {
    case conflict, staged, worktree, untracked

    var title: String {
        switch self {
        case .conflict: "Conflicts"
        case .staged: "Staged"
        case .worktree: "Changes"
        case .untracked: "Untracked"
        }
    }
}

struct DiffFileRow: Decodable, Identifiable, Hashable {
    let path: String
    let old_path: String?
    let layer: String
    let status: String?
    let additions: Int?
    let deletions: Int?
    let binary: Bool?
    let notes: Int?
    let viewed: Bool?
    let fingerprint: String?

    var id: String { "\(layer):\(path)" }
    var layerKind: DiffLayer { DiffLayer(rawValue: layer) ?? .worktree }
    var name: String { path.split(separator: "/").last.map(String.init) ?? path }
    var directory: String {
        guard let slash = path.lastIndex(of: "/") else { return "" }
        return String(path[..<slash])
    }
    /// The daemon reports an untracked file's status as `U`, which Git
    /// reserves for a conflict; the layer tells them apart.
    var statusGlyph: String {
        layerKind == .untracked ? "?" : (status ?? " ")
    }
}

struct DiffListReply: Decodable {
    let repo: String?
    let branch: String?
    let files: [DiffFileRow]
    let omitted: Int?
}

struct DiffLine: Decodable, Hashable {
    let kind: String
    let old_line: Int?
    let new_line: Int?
    let text: String
}

struct DiffHunk: Decodable, Hashable {
    let header: String
    let lines: [DiffLine]?
}

struct DiffFileReply: Decodable {
    let file: DiffFileRow
    let hunks: [DiffHunk]
    let truncated: Bool?
    let omitted_lines: Int?
}

enum NoteSide: String {
    case old, new
}

enum NoteKind: String, CaseIterable, Identifiable {
    case question, issue, suggestion, praise
    var id: String { rawValue }
    var title: String { rawValue.capitalized }
}

/// Empty on purpose: only the count of deliveries is shown.
struct DiffDelivery: Decodable, Hashable {}

struct DiffNote: Decodable, Identifiable, Hashable {
    let id: String
    let path: String
    let layer: String
    let side: String?
    let start_line: Int
    let end_line: Int?
    let kind: String?
    let state: String?
    let body: String
    let deliveries: [DiffDelivery]?

    var noteSide: NoteSide { side == "old" ? .old : .new }
    var lastLine: Int { end_line ?? start_line }
    var isOpen: Bool { (state ?? "open") == "open" }
    var isResolved: Bool { state == "resolved" }
    var sentCount: Int { deliveries?.count ?? 0 }
    var kindTitle: String { NoteKind(rawValue: kind ?? "")?.title ?? "Note" }

    /// `R51–53` for the new side, `L50` for the old.
    var rangeLabel: String {
        let prefix = noteSide == .old ? "L" : "R"
        return lastLine == start_line
            ? "\(prefix)\(start_line)"
            : "\(prefix)\(start_line)–\(lastLine)"
    }
}

struct DiffNotesReply: Decodable {
    let notes: [DiffNote]
    /// The daemon's state revision when the list was read — what an
    /// `if_revision` on a note edit or delete is checked against.
    let revision: Int?
}

struct DiffNoteReply: Decodable {
    let note: DiffNote
}

// MARK: - Rows

/// One drawn row of a file's patch: a hunk header or a line.
struct DiffRow: Identifiable, Hashable {
    enum Kind { case hunk, context, addition, deletion }

    let id: Int
    let kind: Kind
    let old: Int?
    let new: Int?
    let text: String

    func number(on side: NoteSide) -> Int? {
        side == .old ? old : new
    }
}

/// Flatten hunks into rows, in patch order. Tabs become spaces so a
/// proportional fallback never collapses indentation to one cell.
func diffRows(from hunks: [DiffHunk]) -> [DiffRow] {
    var rows: [DiffRow] = []
    for hunk in hunks {
        rows.append(DiffRow(id: rows.count, kind: .hunk, old: nil, new: nil, text: hunk.header))
        for line in hunk.lines ?? [] {
            let kind: DiffRow.Kind
            switch line.kind {
            case "addition": kind = .addition
            case "deletion": kind = .deletion
            default: kind = .context
            }
            rows.append(DiffRow(
                id: rows.count,
                kind: kind,
                old: line.old_line,
                new: line.new_line,
                text: line.text.replacingOccurrences(of: "\t", with: "    ")
            ))
        }
    }
    return rows
}

/// The rows a user has tapped: the first tap anchors, the second extends.
struct LineSelection: Equatable {
    var anchor: Int
    var cursor: Int
}

/// A note's anchor on one side of the diff.
struct NoteRange: Equatable {
    let side: NoteSide
    let start: Int
    let end: Int

    var label: String {
        let prefix = side == .old ? "L" : "R"
        return start == end ? "\(prefix)\(start)" : "\(prefix)\(start)–\(end)"
    }
}

/// Apply a tap to the current selection: a tap on nothing selected anchors,
/// a tap on an anchored line extends, and a tap after a finished range
/// starts a new one.
func selecting(_ row: Int, from current: LineSelection?) -> LineSelection? {
    guard let current else { return LineSelection(anchor: row, cursor: row) }
    if current.anchor == current.cursor {
        // Tapping the anchor again clears it.
        return row == current.anchor ? nil : LineSelection(anchor: current.anchor, cursor: row)
    }
    return LineSelection(anchor: row, cursor: row)
}

/// The note range the selected rows cover on one side. A deletion anchors to
/// the old file and everything else to the new one; rows with no number on
/// that side (an addition, when the side is old) do not extend the range.
func noteRange(rows: [DiffRow], selection: LineSelection) -> NoteRange? {
    guard rows.indices.contains(selection.anchor), rows.indices.contains(selection.cursor) else {
        return nil
    }
    let tapped = rows[selection.cursor]
    guard tapped.kind != .hunk else { return nil }
    var side: NoteSide = tapped.kind == .deletion ? .old : .new
    // A lone line with a number on only one side has only that side to offer.
    if tapped.number(on: side) == nil { side = side == .old ? .new : .old }
    let low = min(selection.anchor, selection.cursor)
    let high = max(selection.anchor, selection.cursor)
    let numbers = rows[low...high]
        .filter { $0.kind != .hunk }
        .compactMap { $0.number(on: side) }
    guard let first = numbers.first else { return nil }
    let start = numbers.reduce(first, min)
    let end = numbers.reduce(first, max)
    return NoteRange(side: side, start: start, end: end)
}

/// The row each note hangs under: the first line numbered `end_line` on the
/// note's side. Notes whose lines are no longer in the patch are returned
/// separately, since their code moved.
func placeNotes(_ notes: [DiffNote], in rows: [DiffRow]) -> (byRow: [Int: [DiffNote]], detached: [DiffNote]) {
    var byRow: [Int: [DiffNote]] = [:]
    var detached: [DiffNote] = []
    for note in notes {
        let at = rows.first { row in
            row.kind != .hunk && row.number(on: note.noteSide) == note.lastLine
        }
        if let at {
            byRow[at.id, default: []].append(note)
        } else {
            detached.append(note)
        }
    }
    return (byRow, detached)
}
