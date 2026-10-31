import SwiftUI
import WebKit

struct NodeWebContainer: View {
  let baseURL: URL
  @State private var loadFailed = false

  var body: some View {
    ZStack {
      NodeWebView(baseURL: baseURL, loadFailed: $loadFailed)
      if loadFailed {
        NodeOfflinePanel(baseURL: baseURL) { loadFailed = false }
      }
    }
  }
}

struct NodeWebView: NSViewRepresentable {
  let baseURL: URL
  @Binding var loadFailed: Bool

  func makeCoordinator() -> Coordinator { Coordinator(loadFailed: $loadFailed) }

  func makeNSView(context: Context) -> WKWebView {
    let config = WKWebViewConfiguration()
    config.preferences.setValue(true, forKey: "developerExtrasEnabled")
    let view = WKWebView(frame: .zero, configuration: config)
    view.navigationDelegate = context.coordinator
    view.load(URLRequest(url: baseURL))
    context.coordinator.webView = view
    NotificationCenter.default.addObserver(
      context.coordinator,
      selector: #selector(Coordinator.reload),
      name: .reloadNodeWeb,
      object: nil
    )
    return view
  }

  func updateNSView(_ nsView: WKWebView, context: Context) {
    guard context.coordinator.lastURL != baseURL else { return }
    context.coordinator.lastURL = baseURL
    loadFailed = false
    nsView.load(URLRequest(url: baseURL))
  }

  final class Coordinator: NSObject, WKNavigationDelegate {
    @Binding var loadFailed: Bool
    weak var webView: WKWebView?
    var lastURL: URL?

    init(loadFailed: Binding<Bool>) {
      _loadFailed = loadFailed
    }

    @objc func reload() {
      loadFailed = false
      if let url = lastURL {
        webView?.load(URLRequest(url: url))
      } else {
        webView?.reload()
      }
    }

    func webView(_ webView: WKWebView, didFailProvisionalNavigation navigation: WKNavigation!, withError error: Error) {
      let ns = error as NSError
      if ns.domain == NSURLErrorDomain && ns.code == NSURLErrorCancelled { return }
      loadFailed = true
    }

    func webView(_ webView: WKWebView, didFinish navigation: WKNavigation!) {
      loadFailed = false
    }
  }
}

struct NodeOfflinePanel: View {
  let baseURL: URL
  let retry: () -> Void

  var body: some View {
    VStack(spacing: 16) {
      Image(nsImage: NSApp.applicationIconImage ?? NSImage(size: NSSize(width: 64, height: 64)))
        .resizable()
        .frame(width: 64, height: 64)
      Text("Cannot reach Node")
        .font(.title2.weight(.semibold))
      Text("Start your local Node, then reload. Default listener: \(baseURL.absoluteString)")
        .multilineTextAlignment(.center)
        .foregroundStyle(.secondary)
        .frame(maxWidth: 420)
      VStack(alignment: .leading, spacing: 8) {
        Label("Install: see docs/install.md (npm or install.sh)", systemImage: "terminal")
        Label("Run: `vibeface` or `vibeface service start`", systemImage: "play.circle")
        Label("Browser check: open the same URL in Safari", systemImage: "safari")
      }
      .font(.callout)
      .frame(maxWidth: 420, alignment: .leading)
      Button("Retry") { retry() }
        .keyboardShortcut(.defaultAction)
    }
    .padding(32)
    .background(.regularMaterial)
  }
}
