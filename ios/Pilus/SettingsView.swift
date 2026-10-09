import SwiftUI

/// Connection info and device identity, reached from Home's ⋯ menu.
/// Pairing lives on its own sheet (`OnboardingView`); this page is read-only
/// state plus the way out.
struct SettingsView: View {
    @Environment(AppStore.self) private var store
    @AppStorage("relayUrl") private var relayUrl = ""
    @AppStorage("apiSecret") private var apiSecret = ""

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    TextField("Relay URL", text: $relayUrl, prompt: Text("https://relay.example.com"))
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .keyboardType(.URL)
                        .font(.system(.body, design: .monospaced))
                    SecureField("API secret", text: $apiSecret)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .font(.system(.body, design: .monospaced))
                } header: {
                    Text("Relay")
                } footer: {
                    Text("Leave empty for the default relays. Applies on next connect.")
                }
                Section {
                    LabeledContent("Name", value: store.hostLabel ?? "—")
                    LabeledContent("Endpoint") {
                        Text(Self.truncated(store.enrolledHost))
                            .font(.system(.caption, design: .monospaced))
                            .textSelection(.enabled)
                            .multilineTextAlignment(.trailing)
                    }
                    if let level = store.level {
                        LabeledContent("Access", value: level)
                    }
                    LabeledContent("Machines", value: "\(store.machines.count)")
                } header: {
                    Text("pilus Host")
                }
                if let deviceId = store.deviceId {
                    Section {
                        Text(deviceId)
                            .font(.system(.caption, design: .monospaced))
                            .textSelection(.enabled)
                    } header: {
                        Text("This device")
                    }
                }
                Section {
                    LabeledContent("Version") {
                        Text(Self.version)
                    }
                } header: {
                    Text("About pilus")
                }
            }
            .navigationTitle("Settings")
        }
    }

    /// `543328......cb308` — enough of both ends to recognize the endpoint
    /// without wrapping the 64-char id across lines.
    private static func truncated(_ endpoint: String?) -> String {
        guard let endpoint, endpoint.count > 18 else { return endpoint ?? "—" }
        return "\(endpoint.prefix(6))......\(endpoint.suffix(5))"
    }

    private static var version: String {
        let info = Bundle.main.infoDictionary
        let version = info?["CFBundleShortVersionString"] as? String ?? "?"
        let build = info?["CFBundleVersion"] as? String ?? "?"
        return "\(version) (\(build))"
    }
}
