import SwiftUI

// `mission.snapshot` / `mission.refresh` — the daemon's Mission Control read
// model. Every field stays optional: a host on an older Luvus leaves keys out,
// and a row whose usage is unknown says why in `usage_status`.

/// A counter the daemon sends as a decimal string (`refresh_id`,
/// `completed_id`) and a later version may send as a number.
struct DecimalID: Decodable {
    let value: Int?

    init(from decoder: Decoder) throws {
        let container = try decoder.singleValueContainer()
        if let number = try? container.decode(Int.self) {
            value = number
        } else if let text = try? container.decode(String.self) {
            value = Int(text)
        } else {
            value = nil
        }
    }
}

struct MissionSnapshot: Decodable {
    struct Summary: Decodable {
        let agents: Int?
        let tokens: Int?
        let cost_usd: Double?
        let burn_usd_per_hour: Double?
        let usage_coverage: Coverage?
    }

    /// How much of the fleet the totals cover. Totals over incomplete coverage
    /// are known subtotals, not the whole.
    struct Coverage: Decodable {
        let available: Int?
        let total: Int?
        let tokens_complete: Bool?
        let cost_complete: Bool?
    }

    struct Usage: Decodable {
        let tokens: Int?
        let cost_usd: Double?
    }

    struct Row: Decodable, Identifiable {
        let agent: String?
        let kind: String?
        let location: String?
        let pane: String?
        let state: String?
        let workspace_name: String?
        let usage: Usage?
        /// Why `usage` is null: no session bound, an unrefreshed cache, an
        /// unsupported reader, or counters the agent does not expose.
        let usage_status: String?

        var id: String { pane ?? location ?? UUID().uuidString }
    }

    struct Refresh: Decodable {
        let server_generation: String?
        let completed_id: DecimalID?
    }

    struct Automation: Decodable {
        struct Summary: Decodable {
            let definitions: Int?
            let running: Int?
            let failed: Int?
            let review: Int?
        }
        let summary: Summary?
    }

    let summary: Summary?
    let rows: [Row]?
    let refresh: Refresh?
    let refreshing: Bool?
    let automation: Automation?
}

struct MissionRefreshReply: Decodable {
    let refresh_id: DecimalID?
    let server_generation: String?
}

extension AppStore {
    /// The Mission Control read for `machine`. It does not open or focus the
    /// Mission tab on the host, and `scope: all` stays inside that machine's
    /// session.
    func missionSnapshot(machine: String) async throws -> MissionSnapshot {
        try await call(method: "mission.snapshot", params: ["scope": "all"], machine: machine)
    }

    /// Ask the host for one bounded usage refresh and wait for it to land.
    ///
    /// The refresh is complete when the snapshot's `refresh.server_generation`
    /// matches the one the request answered with (a daemon restart ends the
    /// wait) and `refresh.completed_id` has reached the requested id. A handful
    /// of reads a second apart, not a poll loop: when the host is slow the
    /// latest snapshot is returned as it stands.
    func refreshMission(machine: String) async throws -> MissionSnapshot {
        let started: MissionRefreshReply = try await call(
            method: "mission.refresh", params: ["scope": "all"], machine: machine)
        let wanted = started.refresh_id?.value ?? 0
        var latest = try await missionSnapshot(machine: machine)
        for _ in 0..<5 {
            try await Task.sleep(for: .seconds(1))
            latest = try await missionSnapshot(machine: machine)
            let generation = latest.refresh?.server_generation
            if generation != started.server_generation { break }
            if (latest.refresh?.completed_id?.value ?? 0) >= wanted, latest.refreshing != true { break }
        }
        return latest
    }
}

/// Fleet usage per machine: the totals the host knows, and for each agent
/// either its usage or the reason there is none.
struct MissionView: View {
    @Environment(AppStore.self) private var store
    @State private var snapshots: [String: MissionSnapshot] = [:]
    @State private var failures: [String: String] = [:]
    @State private var refreshing = false

    var body: some View {
        List {
            ForEach(store.machineOrder, id: \.self) { machine in
                Section {
                    if let snapshot = snapshots[machine] {
                        summaryRows(snapshot)
                        ForEach(snapshot.rows ?? []) { row in
                            rowView(row)
                        }
                    } else if let failure = failures[machine] {
                        Text(failure).font(.footnote).foregroundStyle(.red)
                    } else {
                        ProgressView()
                    }
                } header: {
                    Text(store.machineTitle(machine))
                } footer: {
                    if let coverage = snapshots[machine]?.summary?.usage_coverage,
                       coverage.tokens_complete == false || coverage.cost_complete == false {
                        Text("Totals cover \(coverage.available ?? 0) of \(coverage.total ?? 0) agents; the rest report no usage.")
                    }
                }
            }
        }
        .navigationTitle("Fleet")
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            ToolbarItem(placement: .topBarTrailing) {
                Button {
                    Task { await reload(refresh: true) }
                } label: {
                    if refreshing { ProgressView() } else { Image(systemName: "arrow.clockwise") }
                }
                .disabled(refreshing)
                .accessibilityLabel("Refresh usage")
            }
        }
        .task { await reload(refresh: false) }
        .refreshable { await reload(refresh: true) }
    }

    @ViewBuilder
    private func summaryRows(_ snapshot: MissionSnapshot) -> some View {
        if let summary = snapshot.summary {
            LabeledContent("Agents", value: "\(summary.agents ?? 0)")
            if let tokens = summary.tokens {
                LabeledContent("Tokens", value: tokens.formatted())
            }
            if let cost = summary.cost_usd {
                LabeledContent("Cost", value: cost.formatted(.currency(code: "USD")))
            }
            if let burn = summary.burn_usd_per_hour {
                LabeledContent("Burn", value: "\(burn.formatted(.currency(code: "USD")))/h")
            }
        }
        if let automation = snapshot.automation?.summary, (automation.definitions ?? 0) > 0 {
            LabeledContent(
                "Automations",
                value: "\(automation.running ?? 0) running · \(automation.failed ?? 0) failed · \(automation.review ?? 0) in review")
        }
    }

    private func rowView(_ row: MissionSnapshot.Row) -> some View {
        HStack(alignment: .firstTextBaseline) {
            VStack(alignment: .leading, spacing: 2) {
                Text(row.agent ?? row.kind ?? "agent")
                    .font(.body.weight(.medium))
                Text([row.workspace_name, row.state].compactMap { $0 }.joined(separator: " · "))
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Spacer(minLength: 8)
            if let usage = row.usage {
                VStack(alignment: .trailing, spacing: 2) {
                    if let tokens = usage.tokens { Text("\(tokens.formatted()) tok").font(.caption) }
                    if let cost = usage.cost_usd {
                        Text(cost.formatted(.currency(code: "USD"))).font(.caption).foregroundStyle(.secondary)
                    }
                }
            } else if let status = row.usage_status {
                Text(status.replacingOccurrences(of: "_", with: " "))
                    .font(.caption)
                    .foregroundStyle(.tertiary)
            }
        }
    }

    private func reload(refresh: Bool) async {
        if refresh { refreshing = true }
        defer { refreshing = false }
        for machine in store.machineOrder where store.supports("mission.snapshot", on: machine) {
            do {
                if refresh {
                    snapshots[machine] = try await store.refreshMission(machine: machine)
                } else {
                    snapshots[machine] = try await store.missionSnapshot(machine: machine)
                }
                failures[machine] = nil
            } catch {
                failures[machine] = error.localizedDescription
            }
        }
    }
}
