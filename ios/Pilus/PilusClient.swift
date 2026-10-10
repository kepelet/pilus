import Foundation

/// A failed call, with the daemon's own error code when it answered.
///
/// `code` is what the retry rules branch on: the daemon's (`revision_conflict`,
/// `agent_not_ready`, `stale_server`, …) or one of `transport`, `timeout`,
/// `unconfirmed` (the request may have been applied), `client` (raised here).
struct PilusError: LocalizedError {
    let code: String
    let message: String
    /// The daemon's verdict on a refused terminal input: `rejected` means
    /// nothing was queued.
    var dispatch: String?

    var errorDescription: String? { message }

    /// A client-side failure that never reached the host.
    static func bridge(_ message: String) -> PilusError {
        PilusError(code: "client", message: message)
    }

    /// The connection dropped after the request was sent; a write may have
    /// been applied, so reconcile with a read instead of resending.
    var isUnconfirmed: Bool { code == "unconfirmed" }
    var isRevisionConflict: Bool { code == "revision_conflict" }
    /// The daemon refused an identity (`server_generation`) that moved on; the
    /// input was rejected before it was queued, so refreshing and retrying is
    /// safe.
    var isStaleRuntime: Bool { code == "stale_server" || (dispatch == "rejected" && code.hasPrefix("stale")) }
    var isUnknownMethod: Bool { code == "invalid_request" && message.hasPrefix("unknown method") }
}

/// Thin, blocking wrapper over the pilus-mobile FFI. Call every method from a
/// background task (`Task.detached`); nothing here suspends or hops actors.
/// The Rust side serializes access internally, so sharing is safe.
final class PilusClient: @unchecked Sendable {
    private var handle: OpaquePointer

    private init(handle: OpaquePointer) {
        self.handle = handle
    }

    deinit {
        pilus_client_free(handle)
    }

    static var dataDirectory: URL {
        let root = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
        let dir = root.appendingPathComponent("pilus", isDirectory: true)
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir
    }

    /// Relay settings read fresh on every bind so Settings edits apply on
    /// the next connect without an app restart.
    static var relayUrl: String {
        UserDefaults.standard.string(forKey: "relayUrl") ?? ""
    }
    static var apiSecret: String {
        UserDefaults.standard.string(forKey: "apiSecret") ?? ""
    }

    /// Connect to a host by the `ticket:` line `pilus-agent serve --iroh` prints.
    convenience init(ticket: String) throws {
        let relay = Self.relayUrl, secret = Self.apiSecret
        guard let handle = Self.dataDirectory.path.withCString({ dir in
            ticket.withCString { ticket in
                relay.withCString { relay in
                    secret.withCString { secret in
                        pilus_client_connect(dir, ticket, relay, secret)
                    }
                }
            }
        }) else {
            throw Self.lastError()
        }
        self.init(handle: handle)
    }

    /// Reconnect using the saved enrollment, if there is one.
    static func resume() -> PilusClient? {
        let relay = Self.relayUrl, secret = Self.apiSecret
        return dataDirectory.path.withCString { dir in
            relay.withCString { relay in
                secret.withCString { secret in
                    pilus_client_resume(dir, relay, secret).map { PilusClient(handle: $0) }
                }
            }
        }
    }

    /// This device's endpoint id, stable across restarts. Shown during
    /// onboarding so the user can `--allow` it on the host if they prefer.
    static func deviceId() throws -> String {
        let json = try dataDirectory.path.withCString { dir in
            try takeString(pilus_device_id(dir))
        }
        struct DeviceId: Decodable { let endpoint_id: String }
        return try JSONDecoder().decode(DeviceId.self, from: Data(json.utf8)).endpoint_id
    }

    /// Enroll with the rotating code the host prints, then hold the token.
    func enroll(code: String, label: String) throws {
        let ok = code.withCString { code in
            label.withCString { label in
                pilus_client_enroll(handle, code, label)
            }
        }
        guard ok == 0 else { throw Self.lastError() }
    }

    /// The saved enrollment summary, or nil when this device is not enrolled.
    static func enrollment() -> [String: Any]? {
        guard let json = try? dataDirectory.path.withCString({ dir in
            try takeString(pilus_client_enrollment(dir))
        }) else {
            return nil
        }
        return try? JSONSerialization.jsonObject(with: Data(json.utf8)) as? [String: Any]
    }

    /// One UHP request; returns the reply `result` decoded as JSON.
    func request<Reply: Decodable>(method: String, params: [String: Any] = [:]) throws -> Reply {
        let paramsJSON = String(decoding: try JSONSerialization.data(withJSONObject: params), as: UTF8.self)
        let raw = try method.withCString { method in
            try paramsJSON.withCString { params in
                try Self.takeString(pilus_client_request(handle, method, params))
            }
        }
        return try JSONDecoder().decode(Reply.self, from: Data(raw.utf8))
    }

    /// One hub request (`luvus/hub/1`); `machine` selects the target machine,
    /// `""`/`local` being the paired host. Returns the reply `result` decoded.
    func hubRequest<Reply: Decodable>(
        method: String,
        machine: String = "",
        params: [String: Any] = [:]
    ) throws -> Reply {
        let paramsJSON = String(decoding: try JSONSerialization.data(withJSONObject: params), as: UTF8.self)
        let raw = try machine.withCString { machine in
            try method.withCString { method in
                try paramsJSON.withCString { params in
                    try Self.takeString(pilus_client_hub_request(handle, machine, method, params))
                }
            }
        }
        return try JSONDecoder().decode(Reply.self, from: Data(raw.utf8))
    }

    /// Open `hub.watch` on the paired host; returns the first reply's result.
    func watchOpen() throws -> [String: Any] {
        let raw = try Self.takeString(pilus_hub_watch_open(handle))
        return (try? JSONSerialization.jsonObject(with: Data(raw.utf8))) as? [String: Any] ?? [:]
    }

    /// The next watch signal as a JSON object; throws when the stream ended.
    func watchNext() throws -> [String: Any] {
        let raw = try Self.takeString(pilus_hub_watch_next(handle))
        return (try? JSONSerialization.jsonObject(with: Data(raw.utf8))) as? [String: Any] ?? [:]
    }

    func watchClose() {
        pilus_hub_watch_close(handle)
    }

    static func forget() {
        dataDirectory.path.withCString { dir in
            pilus_forget(dir)
        }
    }

    // MARK: - FFI plumbing

    /// Take ownership of a heap string the Rust side returned, or throw the
    /// recorded last error when it came back NULL.
    private static func takeString(_ raw: UnsafeMutablePointer<CChar>?) throws -> String {
        guard let raw else { throw lastError() }
        defer { pilus_string_free(raw) }
        return String(cString: raw)
    }

    /// The structured error the last failed call recorded on this thread.
    private static func lastError() -> PilusError {
        guard let raw = pilus_last_error_detail() else {
            return PilusError(code: "client", message: "unknown error")
        }
        defer { pilus_string_free(raw) }
        struct Detail: Decodable {
            let code: String
            let message: String
            let dispatch: String?
        }
        guard let detail = try? JSONDecoder().decode(Detail.self, from: Data(String(cString: raw).utf8)) else {
            return PilusError(code: "client", message: String(cString: raw))
        }
        return PilusError(code: detail.code, message: detail.message, dispatch: detail.dispatch)
    }
}
