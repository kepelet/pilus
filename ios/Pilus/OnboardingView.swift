import SwiftUI

struct OnboardingView: View {
    @Environment(AppStore.self) private var store
    /// Home presents this as a sheet for re-pairing — it passes a closer so a
    /// successful connect dismisses the sheet. As the unpaired root it's nil;
    /// the root swap on `isConnected` handles the transition.
    var onPaired: (() -> Void)?

    @State private var ticket = ProcessInfo.processInfo.environment["PILUS_TICKET"] ?? ""
    @State private var code = ProcessInfo.processInfo.environment["PILUS_ENROLL_CODE"] ?? ""
    @State private var label = ProcessInfo.processInfo.environment["PILUS_LABEL"] ?? "iphone"
    @State private var didAutoconnect = false
    @State private var showingManual = false

    var body: some View {
        VStack(spacing: 0) {
            PilusMark()
                .frame(width: 104, height: 104)
                .padding(.top, 56)

            VStack(alignment: .leading, spacing: 12) {
                step(1, "Open pilus on your computer")
                step(2, "Go to Settings → Remote Access → Enable")
                step(3, "Click Add Device → Scan the QR Code")
            }
            .frame(maxWidth: .infinity)
            .padding(.horizontal, 20)
            .padding(.top, 28)

            QRScannerView { scanned in
                guard let pair = PairLink.parse(scanned) else { return }
                Task {
                    await store.connect(
                        ticket: pair.ticket,
                        code: pair.code ?? "",
                        label: pair.name ?? "iphone"
                    )
                    if store.error == nil { onPaired?() }
                }
            }
            .clipShape(RoundedRectangle(cornerRadius: 20, style: .continuous))
            .padding(20)

            if let error = store.error {
                Text(error)
                    .font(.footnote)
                    .foregroundStyle(.red)
                    .padding(.horizontal, 20)
                    .padding(.bottom, 8)
            }

            Spacer(minLength: 0)

            Button("Enter details manually") {
                showingManual = true
            }
            .frame(maxWidth: .infinity)
            .padding(.bottom, 16)
        }
        .sheet(isPresented: $showingManual) {
            ManualPairSheet(ticket: $ticket, code: $code, label: $label, onPaired: onPaired)
        }
        // A deep link pairing another host also counts as success.
        .onChange(of: store.enrolledHost) { _, host in
            if host != nil { onPaired?() }
        }
        // Simulator/dev hook: with PILUS_TICKET + PILUS_ENROLL_CODE in the
        // environment, connect on launch without a tap. Passed through
        // `xcrun simctl launch` as SIMCTL_CHILD_* variables.
        .onAppear {
            guard !didAutoconnect,
                  ProcessInfo.processInfo.environment["PILUS_AUTOCONNECT"] == "1",
                  !ticket.isEmpty, !code.isEmpty else { return }
            didAutoconnect = true
            Task {
                await store.connect(ticket: ticket, code: code, label: label)
            }
        }
    }

    private func step(_ number: Int, _ text: String) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 10) {
            Text("\(number).")
                .font(.body.monospacedDigit())
                .foregroundStyle(.secondary)
            Text(text)
        }
    }
}

private struct ManualPairSheet: View {
    @Environment(AppStore.self) private var store
    @Environment(\.dismiss) private var dismiss
    @Binding var ticket: String
    @Binding var code: String
    @Binding var label: String
    var onPaired: (() -> Void)?

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    TextField("Ticket or pair link", text: $ticket, axis: .vertical)
                        .lineLimit(1...3)
                        .onChange(of: ticket) { applyPairLinkIfPasted() }
                    TextField("Enrollment code", text: $code)
                    TextField("Device label", text: $label)
                }
                .font(.system(.body, design: .monospaced))
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled()

                if let error = store.error {
                    Section {
                        Text(error)
                            .foregroundStyle(.red)
                            .font(.footnote)
                    }
                }
            }
            .navigationTitle("Connect to pilus")
            .navigationBarTitleDisplayMode(.inline)
            .safeAreaInset(edge: .bottom) {
                Button {
                    Task {
                        await store.connect(
                            ticket: ticket.trimmingCharacters(in: .whitespacesAndNewlines),
                            code: code.trimmingCharacters(in: .whitespacesAndNewlines),
                            label: label.isEmpty ? "iphone" : label
                        )
                        if store.error == nil {
                            dismiss()
                            onPaired?()
                        }
                    }
                } label: {
                    Group {
                        if store.busy {
                            ProgressView()
                        } else {
                            Text("Connect")
                        }
                    }
                    .font(.title3.weight(.semibold))
                    .frame(maxWidth: .infinity)
                    .padding(.vertical, 14)
                    .foregroundStyle(.white)
                }
                .modifier(LiquidGlassPanel(radius: 28, prominent: true))
                .disabled(ticket.isEmpty || code.isEmpty || store.busy)
                .padding(.horizontal, 20)
                .padding(.bottom, 8)
            }
        }
        .presentationDetents([.medium, .large])
    }

    /// Pasting a `pilus://pair/…` link into the ticket field fills all three
    /// fields at once instead of making the user split it up.
    private func applyPairLinkIfPasted() {
        guard let pair = PairLink.parse(ticket) else { return }
        ticket = pair.ticket
        code = pair.code ?? code
        if let name = pair.name { label = name }
    }
}
