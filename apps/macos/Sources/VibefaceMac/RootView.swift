import SwiftUI

struct RootView: View {
  @EnvironmentObject private var settings: NodeSettingsStore

  var body: some View {
    NodeWebContainer(baseURL: settings.baseURL)
  }
}

struct SettingsView: View {
  @EnvironmentObject private var settings: NodeSettingsStore
  @State private var draft = ""

  var body: some View {
    Form {
      Section("Local Node") {
        TextField("Base URL", text: $draft, prompt: Text("http://localhost:4790"))
          .textFieldStyle(.roundedBorder)
        Text("Point at the HTTP listener of a running Vibeface Node on this Mac. Default port is 4790.")
          .font(.callout)
          .foregroundStyle(.secondary)
      }
      Section("Launch Node") {
        Text("In Terminal: run `vibeface` after install, or `vibeface service start` for a background service. See docs/install.md.")
          .font(.callout)
          .fixedSize(horizontal: false, vertical: true)
      }
    }
    .formStyle(.grouped)
    .frame(width: 440)
    .onAppear { draft = settings.baseURL.absoluteString }
    .onChange(of: draft) { _, newValue in
      guard let url = URL(string: newValue.trimmingCharacters(in: .whitespacesAndNewlines)),
            let scheme = url.scheme, scheme == "http" || scheme == "https",
            url.host != nil
      else { return }
      settings.baseURL = url
    }
  }
}
