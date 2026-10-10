import UIKit
import UserNotifications

/// Local notifications for agents that start waiting on the user.
///
/// iOS suspends the app soon after it leaves the foreground, and the
/// connection to the host is peer-to-peer with no push relay, so an alert can
/// only fire while the app is still running: in front it would duplicate the
/// blocked dot on the list (so it is skipped), and shortly after backgrounding
/// `BackgroundGrace` keeps the watch alive long enough to deliver one.
@MainActor
enum Notifier {
    static let enabledKey = "notifyBlocked"

    /// On by default; the Settings toggle writes this key.
    static var enabled: Bool {
        UserDefaults.standard.object(forKey: enabledKey) as? Bool ?? true
    }

    /// Ask for permission once the app is in front, where the prompt can be
    /// seen. A later call after the user answered is a no-op.
    static func requestAuthorization() async {
        guard enabled else { return }
        _ = try? await UNUserNotificationCenter.current()
            .requestAuthorization(options: [.alert, .sound])
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
                let content = UNMutableNotificationContent()
                content.title = blocked.workspace.isEmpty ? "pilus" : "pilus · \(blocked.workspace)"
                content.body = "\(blocked.agent) is waiting on you"
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
