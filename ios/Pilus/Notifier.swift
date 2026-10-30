import UIKit
import UserNotifications

/// Alerts for agents that start waiting on the user.
///
/// Two paths, one text ("Claude is waiting your input", the session's name):
///
/// - Push. The paired host watches its agents and, through the push relay,
///   reaches the phone even when iOS has suspended the app. The app's part is
///   to hand the host its APNs token (`AppStore.syncPush`).
/// - Local. The app's own watch posts the alert while it is still running. This
///   covers a host that does not push (an older one, or one with push turned
///   off). In front it would
///   duplicate the blocked dot on the list, so it is skipped, and shortly after
///   backgrounding `BackgroundGrace` keeps the watch alive long enough to
///   deliver one.
///
/// When the host pushes, the local alert steps aside rather than doubling it.
/// The host polls its saved SSH machines too, so that covers them as well.
@MainActor
enum Notifier {
    static let enabledKey = "notifyBlocked"

    /// On by default; the Settings toggle writes this key.
    static var enabled: Bool {
        UserDefaults.standard.object(forKey: enabledKey) as? Bool ?? true
    }

    /// This install's APNs device token, lowercase hex, once iOS has issued it.
    static var token: String? {
        didSet { if token != oldValue { onToken?() } }
    }
    /// Told when `token` changes, so the store can pass it to the host.
    static var onToken: (() -> Void)?

    /// Whether the paired host accepted this phone's token and will push for
    /// its machines. Set from the host's `hub.push.register` reply.
    static var hostPushes = false

    /// Whether this build's token comes from the APNs sandbox. Only a hint:
    /// the relay tries the other environment when it is wrong.
    static var sandbox: Bool {
        #if DEBUG
        true
        #else
        false
        #endif
    }

    /// Ask for permission once the app is in front, where the prompt can be
    /// seen. A later call after the user answered is a no-op. Granted, it also
    /// asks iOS for a push token, which arrives in `PushDelegate`.
    static func requestAuthorization() async {
        guard enabled else { return }
        let granted = (try? await UNUserNotificationCenter.current()
            .requestAuthorization(options: [.alert, .sound])) ?? false
        if granted { UIApplication.shared.registerForRemoteNotifications() }
    }

    static func handle(_ events: [AttentionWatch.Event]) {
        let center = UNUserNotificationCenter.current()
        for event in events {
            switch event {
            case .cleared(let tag):
                center.removePendingNotificationRequests(withIdentifiers: [tag])
                center.removeDeliveredNotifications(withIdentifiers: [tag])
            case .blocked(let blocked):
                guard enabled, UIApplication.shared.applicationState != .active else { continue }
                // The host's push covers its own machine and every saved SSH
                // machine it can reach, which is every machine the app lists.
                if hostPushes { continue }
                let content = UNMutableNotificationContent()
                content.title = "\(blocked.agent) is waiting your input"
                content.body = blocked.session
                content.sound = .default
                // Reusing the tag as the identifier makes a newer alert for
                // the same pane replace the older one.
                center.add(UNNotificationRequest(identifier: blocked.tag, content: content, trigger: nil))
            }
        }
    }

    static func withdrawAll() {
        let center = UNUserNotificationCenter.current()
        center.removeAllPendingNotificationRequests()
        center.removeAllDeliveredNotifications()
    }
}

/// Buys the watch a short grace period after the app is backgrounded, so an
/// agent that blocks just after the user switches away still notifies.
/// iOS grants roughly 30 seconds; past that the app is suspended.
@MainActor
enum BackgroundGrace {
    private static var task = UIBackgroundTaskIdentifier.invalid

    static func begin() {
        guard task == .invalid else { return }
        task = UIApplication.shared.beginBackgroundTask(withName: "pilus.watch") { end() }
    }

    static func end() {
        guard task != .invalid else { return }
        UIApplication.shared.endBackgroundTask(task)
        task = .invalid
    }
}

/// Receives the APNs token. SwiftUI's `App` has no hook for it, so the app
/// installs this through `UIApplicationDelegateAdaptor`.
final class PushDelegate: NSObject, UIApplicationDelegate {
    func application(
        _ application: UIApplication,
        didRegisterForRemoteNotificationsWithDeviceToken deviceToken: Data
    ) {
        let token = deviceToken.map { String(format: "%02x", $0) }.joined()
        Task { @MainActor in Notifier.token = token }
    }

    func application(
        _ application: UIApplication,
        didFailToRegisterForRemoteNotificationsWithError error: Error
    ) {
        // No token means no push; the local alerts keep working.
        Task { @MainActor in Notifier.token = nil }
    }
}
