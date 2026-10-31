import SwiftUI

@main
struct VibefaceMacApp: App {
  @NSApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
  @StateObject private var settings = NodeSettingsStore.shared

  var body: some Scene {
    WindowGroup("Vibeface") {
      RootView()
        .environmentObject(settings)
        .frame(minWidth: 960, minHeight: 640)
    }
    .commands {
      CommandGroup(replacing: .newItem) {}
      CommandMenu("Node") {
        Button("Reload page") { NotificationCenter.default.post(name: .reloadNodeWeb, object: nil) }
          .keyboardShortcut("r", modifiers: [.command])
        Button("Use default URL (localhost:4790)") { settings.resetToDefault() }
        Divider()
        Button("Open getting started…") { settings.openGettingStarted() }
      }
    }
    Settings {
      SettingsView()
        .environmentObject(settings)
    }
  }
}

final class AppDelegate: NSObject, NSApplicationDelegate {
  func applicationDidFinishLaunching(_ notification: Notification) {
    NSApp.setActivationPolicy(.regular)
    NSApp.activate(ignoringOtherApps: true)
  }
}

extension Notification.Name {
  static let reloadNodeWeb = Notification.Name("vibeface.reloadNodeWeb")
}
