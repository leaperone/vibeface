import ExpoModulesCore
import Network

/// Finds `_vibeface._udp` services on this LAN while JS listens, each resolved to one IPv4 `ip:port`.
public class LanBrowserModule: Module {
  private let queue = DispatchQueue(label: "one.leaper.vibeface.lan-browser")
  private var browser: NWBrowser?
  /// Resolved services by instance name, as JS gets them.
  private var found: [String: [String: Any]] = [:]
  /// What the browser sees now, by instance name.
  private var seen: [String: (tag: String, endpoint: NWEndpoint)] = [:]
  private var resolving: [String: NWConnection] = [:]
  /// Bonjour does not report a service whose address changed, so each one is resolved again on this beat.
  private var refresh: DispatchSourceTimer?

  public func definition() -> ModuleDefinition {
    Name("LanBrowser")
    Events("onChange")
    OnStartObserving("onChange") { self.queue.async { self.start() } }
    OnStopObserving("onChange") { self.queue.async { self.stop() } }
  }

  // ponytail: a browse that fails stays stopped until JS listens again, on the next return to the foreground.
  private func start() {
    guard browser == nil else { return }
    let browser = NWBrowser(for: .bonjourWithTXTRecord(type: "_vibeface._udp", domain: nil), using: NWParameters())
    browser.browseResultsChangedHandler = { [weak self] results, _ in self?.update(results) }
    self.browser = browser
    browser.start(queue: queue)
    let refresh = DispatchSource.makeTimerSource(queue: queue)
    refresh.schedule(deadline: .now() + 60, repeating: 60)
    refresh.setEventHandler { [weak self] in
      guard let self else { return }
      for (name, service) in self.seen where self.resolving[name] == nil { self.resolve(name, service.tag, service.endpoint) }
    }
    refresh.resume()
    self.refresh = refresh
  }

  private func stop() {
    browser?.cancel()
    browser = nil
    refresh?.cancel()
    refresh = nil
    seen = [:]
    resolving.values.forEach(drop)
    resolving = [:]
    found = [:]
  }

  private func update(_ results: Set<NWBrowser.Result>) {
    var now: [String: (tag: String, endpoint: NWEndpoint)] = [:]
    for result in results {
      guard case let .service(name, _, _, _) = result.endpoint, case let .bonjour(txt) = result.metadata, let tag = txt["t"] else { continue }
      now[name] = (tag, result.endpoint)
      if found[name]?["tag"] as? String != tag && resolving[name] == nil { resolve(name, tag, result.endpoint) }
    }
    seen = now
    for name in Array(resolving.keys) where now[name] == nil {
      if let connection = resolving.removeValue(forKey: name) { drop(connection) }
    }
    let gone = found.keys.filter { now[$0] == nil }
    gone.forEach { found[$0] = nil }
    if !gone.isEmpty { emit() }
  }

  /// Bonjour names the service, not its address: a UDP connection to it resolves one and sends nothing.
  private func resolve(_ name: String, _ tag: String, _ endpoint: NWEndpoint) {
    let parameters = NWParameters.udp
    // An IPv6 link-local address needs its interface, which an `ip:port` cannot carry.
    (parameters.defaultProtocolStack.internetProtocol as? NWProtocolIP.Options)?.version = .v4
    let connection = NWConnection(to: endpoint, using: parameters)
    resolving[name] = connection
    connection.stateUpdateHandler = { [weak self] state in
      guard let self else { return }
      switch state {
      case .ready:
        if case let .hostPort(host, port) = connection.currentPath?.remoteEndpoint, case let .ipv4(address) = host {
          let ip = address.rawValue.map(String.init).joined(separator: ".")
          let addrs = ["\(ip):\(port.rawValue)"]
          if self.found[name]?["tag"] as? String != tag || self.found[name]?["addrs"] as? [String] != addrs {
            self.found[name] = ["tag": tag, "addrs": addrs]
            self.emit()
          }
        }
        self.finish(name, connection)
      case .waiting, .failed:
        self.finish(name, connection)
      default:
        break
      }
    }
    connection.start(queue: queue)
  }

  private func finish(_ name: String, _ connection: NWConnection) {
    if resolving[name] === connection { resolving[name] = nil }
    drop(connection)
  }

  /// The handler holds its connection; clearing it lets both go.
  private func drop(_ connection: NWConnection) {
    connection.stateUpdateHandler = nil
    connection.cancel()
  }

  private func emit() {
    sendEvent("onChange", ["services": Array(found.values)])
  }
}
