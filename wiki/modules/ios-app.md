---
title: iOS app
description: The SwiftUI client, its state, screens, level gating, and Rust build.
tags:
  - wiki
  - module
---
# iOS app

## Summary

`ios/` is the SwiftUI client for pilus. It links the Rust `pilus-mobile` static library through a C bridging header, enrolls with a host by scanning its pairing QR code, and then drives the host's hub: workspaces, agents, tasks, Mission Control, diffs, GitHub, and an agent conversation view. It targets iOS 17 and is built with XcodeGen from [project.yml](../../ios/project.yml).

## Responsibilities

- Own the app shell and onboarding ([PilusApp.swift](../../ios/Pilus/PilusApp.swift), [OnboardingView.swift](../../ios/Pilus/OnboardingView.swift)).
- Keep merged state across every reachable machine ([AppStore.swift](../../ios/Pilus/AppStore.swift)).
- Wrap the FFI in a blocking, sendable client ([PilusClient.swift](../../ios/Pilus/PilusClient.swift)).
- Render the main surfaces: Home, agent detail, diff review, GitHub, Git log, Mission Control, composers, settings, notifications.
- Handle `pilus://pair/…` links and QR scanning ([QRScannerView.swift](../../ios/Pilus/QRScannerView.swift)).
- Watch for attention and post notifications ([AttentionWatch.swift](../../ios/Pilus/AttentionWatch.swift), [Notifier.swift](../../ios/Pilus/Notifier.swift)).

## Public API / entry points

The app entry is `PilusApp` with a `WindowGroup`; `AppStore` is the `@MainActor @Observable` state object injected into the environment. A cold launch shows `OnboardingView`; a connected store shows `HomeView`. A `pilus://pair/<ticket>?code=…` URL pairs directly via `.onOpenURL`.

`PilusClient` is a thin wrapper over the C ABI: every call is blocking and must run from `Task.detached`; the Rust side serializes access internally. Errors arrive as `PilusError { code, message, dispatch }`, and the app branches on the daemon's codes (`revision_conflict`, `agent_not_ready`, `stale_server`, `unconfirmed`) rather than message text.

## Key files

| Concern | Files |
| --- | --- |
| Shell and onboarding | `PilusApp.swift`, `OnboardingView.swift`, `QRScannerView.swift`, `PilusMark.swift` |
| State and transport | `AppStore.swift`, `PilusClient.swift`, `Models.swift`, `AttentionWatch.swift`, `Notifier.swift` |
| Home and launch | `HomeView.swift`, `HomeComposerView.swift`, `ComposeSheetView.swift`, `NewAgentView.swift`, `NewTaskView.swift`, `OpenWorkspaceView.swift` |
| Agents and work | `AgentDetailView.swift`, `MissionView.swift`, `GitLogView.swift`, `GitHubView.swift` |
| Review | `DiffReviewView.swift`, `DiffModels.swift`, `ANSIText.swift` |
| Settings | `SettingsView.swift`, `NotificationsView.swift` |

## Build

[project.yml](../../ios/project.yml) declares a pre-build script that runs [build-rust.sh](../../ios/build-rust.sh), which selects the Rust target for `iphoneos` or `iphonesimulator` and builds `pilus-mobile`. The app links `libpilus_mobile.a` from `target/<triple>/debug`, with `Network.framework` and `SystemConfiguration.framework` (iroh's netdev/netwatch call into them). `INFOPLIST_KEY_ITSAppUsesNonExemptEncryption: NO` is valid because the app only uses QUIC/TLS.

## Behaviour worth knowing

- **Level-aware UI.** The hub rewrites `uhp.capabilities` per device, and the app gates features on `allowedMethods` and `level`: diff viewing vs review, GitHub reads vs writes, and operate-only actions.
- **Writes are not blindly retried.** A dropped reply is reported `unconfirmed`; only reads and calls carrying an idempotency key are resent, while a prompt, note, or workspace open keeps the user's text and reads state back.
- **Watch drives freshness.** The app holds `hub.watch`, refetches only the lists a signal names for the paired machine, re-reads an agent's screen when its pane is named, and resyncs everything on reconnect or a backwards `seq`.
- **Attention.** `AttentionWatch` spots an agent becoming blocked or done and posts a local notification; background grace is handled on scene phase.

## Dependencies

SwiftUI, Foundation, AVFoundation (QR), UserNotifications, Network, SystemConfiguration; the Rust `pilus-mobile` static library; XcodeGen for project generation.

## Related

- [pilus-mobile](./mobile.md), [pilus-agent](./agent.md)
- [Pair a phone](../flows/phone-pairing.md)
- [Enrollment level](../concepts/enrollment-level.md)
- [Remote access and the hub](../architecture/remote-access.md)
