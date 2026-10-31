import AppKit
import Foundation

@MainActor
final class NodeSettingsStore: ObservableObject {
  static let shared = NodeSettingsStore()

  private static let urlKey = "vibeface.nodeBaseURL"
  static let defaultBaseURL = URL(string: "http://localhost:4790")!

  @Published var baseURL: URL {
    didSet { UserDefaults.standard.set(baseURL.absoluteString, forKey: Self.urlKey) }
  }

  private init() {
    if let raw = UserDefaults.standard.string(forKey: Self.urlKey), let url = URL(string: raw), url.scheme == "http" || url.scheme == "https" {
      baseURL = url
    } else {
      baseURL = Self.defaultBaseURL
    }
  }

  func resetToDefault() { baseURL = Self.defaultBaseURL }

  func openGettingStarted() {
    if let url = URL(string: "https://github.com/leaperone/vibeface/blob/main/docs/macos.md") {
      NSWorkspace.shared.open(url)
    }
  }
}
