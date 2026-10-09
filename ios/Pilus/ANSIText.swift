import SwiftUI

/// Turns a `terminal.backend.capture` (`ansi: true`) payload into an
/// `AttributedString` so the agent screen keeps its real colors.
///
/// Scope is the SGR subset agents actually emit: reset, bold, dim, italic,
/// underline, inverse, the 8/16 named colors, `38;5;n` palette indexes, and
/// `38;2;r;g;b` truecolor (and the `48;…` background twins). Every other
/// escape — cursor moves, OSC hyperlinks, charset switches — is skipped
/// because a screen snapshot is already the post-layout text.
enum ANSIRenderer {

    /// The conventional xterm palette. These are the terminal's colors, not
    /// the app's theme — agent output assumes them whatever the chrome looks
    /// like.
    private static let named: [Color] = [
        .init(.sRGB, red: 0.00, green: 0.00, blue: 0.00), // black
        .init(.sRGB, red: 0.80, green: 0.19, blue: 0.19), // red
        .init(.sRGB, red: 0.05, green: 0.74, blue: 0.47), // green
        .init(.sRGB, red: 0.90, green: 0.90, blue: 0.06), // yellow
        .init(.sRGB, red: 0.14, green: 0.45, blue: 0.78), // blue
        .init(.sRGB, red: 0.74, green: 0.25, blue: 0.74), // magenta
        .init(.sRGB, red: 0.07, green: 0.66, blue: 0.80), // cyan
        .init(.sRGB, red: 0.90, green: 0.90, blue: 0.90), // white
        .init(.sRGB, red: 0.40, green: 0.40, blue: 0.40), // bright black
        .init(.sRGB, red: 0.95, green: 0.30, blue: 0.30), // bright red
        .init(.sRGB, red: 0.14, green: 0.82, blue: 0.55), // bright green
        .init(.sRGB, red: 0.96, green: 0.96, blue: 0.26), // bright yellow
        .init(.sRGB, red: 0.23, green: 0.56, blue: 0.92), // bright blue
        .init(.sRGB, red: 0.84, green: 0.44, blue: 0.84), // bright magenta
        .init(.sRGB, red: 0.16, green: 0.72, blue: 0.86), // bright cyan
        .init(.sRGB, red: 1.00, green: 1.00, blue: 1.00), // bright white
    ]

    /// xterm's 256-entry palette index → color: the named 0–15, then the
    /// 6×6×6 color cube, then the 24-step grayscale ramp.
    private static func palette(_ index: Int) -> Color? {
        switch index {
        case 0...15:
            return named[index]
        case 16...231:
            let cube = index - 16
            let steps: [Double] = [0, 95, 135, 175, 215, 255]
            return Color(.sRGB,
                         red: steps[cube / 36] / 255,
                         green: steps[(cube / 6) % 6] / 255,
                         blue: steps[cube % 6] / 255)
        case 232...255:
            let level = Double(8 + (index - 232) * 10) / 255
            return Color(.sRGB, red: level, green: level, blue: level)
        default:
            return nil
        }
    }

    /// Styling state for the run being accumulated, mutated per SGR sequence.
    private struct Style {
        var foreground: Color?
        var background: Color?
        var bold = false
        var dim = false
        var italic = false
        var underline = false
        var inverse = false
    }

    static func attributed(_ source: String, size: CGFloat = 12) -> AttributedString {
        var result = AttributedString()
        var style = Style()
        var buffer = ""

        func flush() {
            guard !buffer.isEmpty else { return }
            var run = AttributedString(buffer)
            buffer = ""

            var foreground = style.foreground
            var background = style.background
            if style.inverse { swap(&foreground, &background) }
            if style.dim {
                foreground = (foreground ?? Color(.label)).opacity(0.55)
            }
            if let foreground { run.foregroundColor = foreground }
            if let background { run.backgroundColor = background }
            if style.underline { run.underlineStyle = .single }
            if style.bold || style.italic {
                var font = Font.system(
                    size: size,
                    weight: style.bold ? .semibold : .regular,
                    design: .monospaced)
                if style.italic { font = font.italic() }
                run.font = font
            }
            result.append(run)
        }

        /// `38;5;n` palette or `38;2;r;g;b` truecolor — `params[index]` is the
        /// 38/48 already consumed; moves `index` past whatever it reads.
        func extended(_ params: [Int], _ index: inout Int) -> Color? {
            guard index + 1 < params.count else { return nil }
            switch params[index + 1] {
            case 5:
                guard index + 2 < params.count else { return nil }
                index += 2
                return palette(params[index])
            case 2:
                // Tolerate both `38;2;r;g;b` and the colorspace-id variant
                // `38;2;c;r;g;b` some emitters produce.
                let start = index + 2
                let rgb = params.count - start >= 4 && params[start] <= 5
                    ? start + 1
                    : start
                guard rgb + 2 < params.count else { return nil }
                index = rgb + 2
                return Color(.sRGB,
                             red: Double(params[rgb]) / 255,
                             green: Double(params[rgb + 1]) / 255,
                             blue: Double(params[rgb + 2]) / 255)
            default:
                return nil
            }
        }

        /// One SGR list like `0;1;38;5;196`. Empty means reset.
        func applySGR(_ body: String) {
            let params = body.isEmpty ? [0] : body
                .split(separator: ";", omittingEmptySubsequences: false)
                .map { Int($0) ?? 0 }
            var index = 0
            while index < params.count {
                let code = params[index]
                switch code {
                case 0: style = Style()
                case 1: style.bold = true
                case 2: style.dim = true
                case 3: style.italic = true
                case 4: style.underline = true
                case 7: style.inverse = true
                case 22: style.bold = false; style.dim = false
                case 23: style.italic = false
                case 24: style.underline = false
                case 27: style.inverse = false
                case 30...37: style.foreground = named[code - 30]
                case 38, 48:
                    if code == 38 {
                        style.foreground = extended(params, &index)
                    } else {
                        style.background = extended(params, &index)
                    }
                case 39: style.foreground = nil
                case 40...47: style.background = named[code - 40]
                case 49: style.background = nil
                case 90...97: style.foreground = named[code - 90 + 8]
                case 100...107: style.background = named[code - 100 + 8]
                default: break
                }
                index += 1
            }
        }

        var index = source.startIndex
        while index < source.endIndex {
            let character = source[index]
            guard character == "\u{1B}" else {
                buffer.append(character)
                index = source.index(after: index)
                continue
            }
            flush()

            var cursor = source.index(after: index)
            guard cursor < source.endIndex else { break }

            switch source[cursor] {
            case "[":
                // CSI: parameters/intermediates, then one final byte @-~.
                cursor = source.index(after: cursor)
                let bodyStart = cursor
                while cursor < source.endIndex {
                    let scalar = source[cursor].unicodeScalars.first?.value ?? 0
                    if (0x40...0x7E).contains(scalar) { break }
                    cursor = source.index(after: cursor)
                }
                guard cursor < source.endIndex else { break }
                if source[cursor] == "m" {
                    // Colon subparameters (`38:5:n`) arrive as one SGR list.
                    let body = source[bodyStart..<cursor]
                        .replacingOccurrences(of: ":", with: ";")
                        .replacingOccurrences(of: "?", with: "")
                    applySGR(body)
                }
                index = source.index(after: cursor)

            case "]":
                // OSC: titles/hyperlinks, terminated by BEL or ST — not text.
                cursor = source.index(after: cursor)
                while cursor < source.endIndex {
                    if source[cursor] == "\u{7}" {
                        cursor = source.index(after: cursor)
                        break
                    }
                    if source[cursor] == "\u{1B}" {
                        let next = source.index(after: cursor)
                        if next < source.endIndex && source[next] == "\\" {
                            cursor = source.index(after: next)
                            break
                        }
                    }
                    cursor = source.index(after: cursor)
                }
                index = cursor

            case "(", ")", "*", "+":
                // Charset designation: ESC + designator + one charset char.
                index = source.index(cursor, offsetBy: 2,
                                     limitedBy: source.endIndex)
                    ?? source.endIndex

            default:
                // Two-byte escape (ESC + one char): skip both.
                index = source.index(after: cursor)
            }
        }
        flush()
        return result
    }
}
