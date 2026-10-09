import SwiftUI

@main
struct PilusApp: App {
    @State private var store = AppStore()

    var body: some Scene {
        WindowGroup {
            Group {
                if store.isConnected {
                    HomeView()
                } else {
                    OnboardingView()
                }
            }
            // A `pilus://pair/…` link opened from outside — a QR in another
            // app, a message, the host's printed URL — pairs directly.
            .onOpenURL { url in
                Task { _ = await store.connect(pairLink: url.absoluteString) }
            }
        }
        .environment(store)
    }
}
