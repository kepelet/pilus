import SwiftUI

/// The pilus mark as drawn on usepilus.dev: a blob body with two eyes that
/// wander and blink. Drawn natively so the colors follow the appearance —
/// dark body with light eyes in light mode, inverted in dark mode.
struct PilusMark: View {
    var animated = true

    @State private var eyeOffset = CGSize.zero
    @State private var blinking = false

    private static let leftEyeAnchor = UnitPoint(x: 0.405, y: 0.46)
    private static let rightEyeAnchor = UnitPoint(x: 0.597, y: 0.4515)

    var body: some View {
        GeometryReader { geo in
            ZStack {
                PilusBodyShape()
                    .fill(Color.primary)
                PilusLeftEyeShape()
                    .fill(Color(.systemBackground))
                    .scaleEffect(x: 1, y: blinking ? 0.08 : 1, anchor: Self.leftEyeAnchor)
                PilusRightEyeShape()
                    .fill(Color(.systemBackground))
                    .scaleEffect(x: 1, y: blinking ? 0.08 : 1, anchor: Self.rightEyeAnchor)
            }
            // Wander distance is authored on the mark's 100-unit canvas.
            .offset(
                x: eyeOffset.width * geo.size.width / 100,
                y: eyeOffset.height * geo.size.height / 100
            )
        }
        .aspectRatio(1, contentMode: .fit)
        .task { await wander() }
        .task { await blinkLoop() }
    }

    /// The site nudges the eyes ±5px/±3px on a 100-unit mark every 0.7–2.6s.
    private func wander() async {
        while animated && !Task.isCancelled {
            try? await Task.sleep(for: .milliseconds(Int.random(in: 700...2600)))
            guard !Task.isCancelled else { return }
            withAnimation(.timingCurve(0.16, 1, 0.3, 1, duration: 0.28)) {
                eyeOffset = CGSize(
                    width: CGFloat.random(in: -5...5),
                    height: CGFloat.random(in: -3...3)
                )
            }
        }
    }

    /// …and blinks for ~90ms every 1.4–4.2s.
    private func blinkLoop() async {
        while animated && !Task.isCancelled {
            try? await Task.sleep(for: .milliseconds(Int.random(in: 1400...4200)))
            guard !Task.isCancelled else { return }
            withAnimation(.easeInOut(duration: 0.09)) { blinking = true }
            try? await Task.sleep(for: .milliseconds(90))
            withAnimation(.easeInOut(duration: 0.09)) { blinking = false }
        }
    }
}

private struct PilusBodyShape: Shape {
    func path(in rect: CGRect) -> Path {
        path100(rect) { p in
            p.move(to: CGPoint(x: 81.28, y: 50.19))
            p.addCurve(to: CGPoint(x: 49.82, y: 79.47),
                       control1: CGPoint(x: 81.28, y: 69.32),
                       control2: CGPoint(x: 70.37, y: 79.47))
            p.addCurve(to: CGPoint(x: 18.36, y: 50.19),
                       control1: CGPoint(x: 29.27, y: 79.47),
                       control2: CGPoint(x: 18.36, y: 69.32))
            p.addCurve(to: CGPoint(x: 49.82, y: 20.92),
                       control1: CGPoint(x: 18.36, y: 31.07),
                       control2: CGPoint(x: 29.27, y: 20.92))
            p.addCurve(to: CGPoint(x: 81.28, y: 50.19),
                       control1: CGPoint(x: 70.37, y: 20.92),
                       control2: CGPoint(x: 81.28, y: 31.07))
            p.closeSubpath()
        }
    }
}

private struct PilusLeftEyeShape: Shape {
    func path(in rect: CGRect) -> Path {
        path100(rect) { p in
            p.move(to: CGPoint(x: 42.98, y: 46.05))
            p.addCurve(to: CGPoint(x: 40.21, y: 51.14),
                       control1: CGPoint(x: 42.72, y: 50.76),
                       control2: CGPoint(x: 42.44, y: 51.27))
            p.addCurve(to: CGPoint(x: 38.01, y: 45.77),
                       control1: CGPoint(x: 37.97, y: 51.02),
                       control2: CGPoint(x: 37.75, y: 50.48))
            p.addCurve(to: CGPoint(x: 40.79, y: 40.67),
                       control1: CGPoint(x: 38.27, y: 41.06),
                       control2: CGPoint(x: 38.55, y: 40.55))
            p.addCurve(to: CGPoint(x: 42.98, y: 46.05),
                       control1: CGPoint(x: 43.03, y: 40.8),
                       control2: CGPoint(x: 43.25, y: 41.33))
            p.closeSubpath()
        }
    }
}

private struct PilusRightEyeShape: Shape {
    func path(in rect: CGRect) -> Path {
        path100(rect) { p in
            p.move(to: CGPoint(x: 62.69, y: 45.07))
            p.addCurve(to: CGPoint(x: 59.56, y: 50.34),
                       control1: CGPoint(x: 62.47, y: 49.93),
                       control2: CGPoint(x: 62.16, y: 50.46))
            p.addCurve(to: CGPoint(x: 56.93, y: 44.81),
                       control1: CGPoint(x: 56.97, y: 50.22),
                       control2: CGPoint(x: 56.7, y: 49.66))
            p.addCurve(to: CGPoint(x: 60.06, y: 39.54),
                       control1: CGPoint(x: 57.15, y: 39.95),
                       control2: CGPoint(x: 57.46, y: 39.43))
            p.addCurve(to: CGPoint(x: 62.69, y: 45.07),
                       control1: CGPoint(x: 62.65, y: 39.66),
                       control2: CGPoint(x: 62.92, y: 40.22))
            p.closeSubpath()
        }
    }
}

/// The mark's paths are authored on a 100×100 canvas; scale into `rect`.
private func path100(_ rect: CGRect, _ build: (inout Path) -> Void) -> Path {
    var p = Path()
    build(&p)
    let scale = min(rect.width, rect.height) / 100
    return p.applying(CGAffineTransform(scaleX: scale, y: scale))
}
