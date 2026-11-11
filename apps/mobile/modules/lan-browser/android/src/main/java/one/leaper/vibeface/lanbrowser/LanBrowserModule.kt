package one.leaper.vibeface.lanbrowser

import android.content.Context
import android.net.nsd.NsdManager
import android.net.nsd.NsdServiceInfo
import android.net.wifi.WifiManager
import android.os.Build
import expo.modules.kotlin.modules.Module
import expo.modules.kotlin.modules.ModuleDefinition
import java.net.Inet4Address
import java.net.InetAddress
import java.util.concurrent.Executors

/** Finds `_vibeface._udp` services on this LAN while JS listens, each resolved to its IPv4 `ip:port`. */
class LanBrowserModule : Module() {
  private val lock = Any()
  private var discovery: NsdManager.DiscoveryListener? = null
  private var multicast: WifiManager.MulticastLock? = null
  /** Resolved services by instance name, as JS gets them. */
  private val found = mutableMapOf<String, Map<String, Any>>()
  /** Android 14+: one callback per service, which also follows its address changes. */
  private val callbacks = mutableMapOf<String, NsdManager.ServiceInfoCallback>()
  /**
   * Older Android resolves one service at a time, once.
   * ponytail: an address change shows up when the Node's hourly name change does; resolve on a beat if that is too slow.
   */
  private val waiting = ArrayDeque<Pair<DiscoveryLifetime.Ticket, NsdServiceInfo>>()
  private val lifetime = DiscoveryLifetime()
  private var resolving: DiscoveryLifetime.Ticket? = null
  private val executor = Executors.newSingleThreadExecutor()

  override fun definition() = ModuleDefinition {
    Name("LanBrowser")
    Events("onChange")
    OnStartObserving("onChange") { start() }
    OnStopObserving("onChange") { stop() }
  }

  private val context: Context get() = requireNotNull(appContext.reactContext).applicationContext
  private val nsd: NsdManager get() = context.getSystemService(Context.NSD_SERVICE) as NsdManager

  private fun start() {
    synchronized(lock) {
      if (discovery != null) return
      // Wi-Fi drops multicast, mDNS included, unless an app holds this.
      val wifi = context.getSystemService(Context.WIFI_SERVICE) as WifiManager
      multicast = wifi.createMulticastLock("vibeface-lan").apply { setReferenceCounted(false); acquire() }
      val session = lifetime.start()
      val listener = object : NsdManager.DiscoveryListener {
        override fun onDiscoveryStarted(serviceType: String) {}
        override fun onDiscoveryStopped(serviceType: String) {}
        override fun onStartDiscoveryFailed(serviceType: String, errorCode: Int) {}
        override fun onStopDiscoveryFailed(serviceType: String, errorCode: Int) {}
        override fun onServiceFound(info: NsdServiceInfo) = resolve(session, info)
        override fun onServiceLost(info: NsdServiceInfo) = lost(session, info.serviceName)
      }
      discovery = listener
      nsd.discoverServices("_vibeface._udp", NsdManager.PROTOCOL_DNS_SD, listener)
    }
  }

  private fun stop() {
    synchronized(lock) {
      val listener = discovery ?: return
      discovery = null
      lifetime.stop()
      runCatching { nsd.stopServiceDiscovery(listener) }
      if (Build.VERSION.SDK_INT >= 34) callbacks.values.forEach { runCatching { nsd.unregisterServiceInfoCallback(it) } }
      callbacks.clear()
      waiting.clear()
      found.clear()
      multicast?.release()
      multicast = null
    }
  }

  private fun resolve(session: Long, info: NsdServiceInfo) {
    synchronized(lock) {
      val ticket = lifetime.find(session, info.serviceName) ?: return
      if (Build.VERSION.SDK_INT < 34) {
        waiting.addLast(ticket to info)
        next()
        return
      }
      if (callbacks.containsKey(info.serviceName)) return
      val callback = object : NsdManager.ServiceInfoCallback {
        override fun onServiceInfoCallbackRegistrationFailed(errorCode: Int) {
          synchronized(lock) { if (lifetime.current(ticket)) lost(session, ticket.name) }
        }
        override fun onServiceUpdated(service: NsdServiceInfo) = put(ticket, service.attributes["t"], service.hostAddresses, service.port)
        override fun onServiceLost() {
          synchronized(lock) { if (lifetime.current(ticket)) lost(session, ticket.name) }
        }
        override fun onServiceInfoCallbackUnregistered() {}
      }
      callbacks[info.serviceName] = callback
      try { nsd.registerServiceInfoCallback(info, executor, callback) }
      catch (failure: RuntimeException) { lost(session, info.serviceName) }
    }
  }

  /** Runs under `lock`. */
  @Suppress("DEPRECATION")
  private fun next() {
    if (resolving != null || discovery == null) return
    val (ticket, info) = waiting.removeFirstOrNull() ?: return
    if (!lifetime.current(ticket)) { next(); return }
    resolving = ticket
    try { nsd.resolveService(info, object : NsdManager.ResolveListener {
      override fun onResolveFailed(service: NsdServiceInfo, errorCode: Int) {
        synchronized(lock) { if (lifetime.current(ticket)) lifetime.lose(ticket.session, ticket.name) }
        done(ticket)
      }
      override fun onServiceResolved(service: NsdServiceInfo) {
        put(ticket, service.attributes["t"], listOfNotNull(service.host), service.port)
        done(ticket)
      }
    }) } catch (failure: RuntimeException) {
      if (lifetime.current(ticket)) lifetime.lose(ticket.session, ticket.name)
      done(ticket)
    }
  }

  private fun done(ticket: DiscoveryLifetime.Ticket) {
    synchronized(lock) {
      if (resolving != ticket) return
      resolving = null
      next()
    }
  }

  private fun put(ticket: DiscoveryLifetime.Ticket, tag: ByteArray?, hosts: List<InetAddress>, port: Int) {
    synchronized(lock) {
      val addrs = hosts.filterIsInstance<Inet4Address>().mapNotNull { it.hostAddress }.map { "$it:$port" }
      if (!lifetime.current(ticket) || tag == null || addrs.isEmpty()) return
      found[ticket.name] = mapOf("tag" to String(tag, Charsets.UTF_8), "addrs" to addrs)
      emit()
    }
  }

  private fun lost(session: Long, name: String) {
    synchronized(lock) {
      val ticket = lifetime.lose(session, name) ?: return
      waiting.removeAll { it.first == ticket }
      if (Build.VERSION.SDK_INT >= 34) callbacks.remove(name)?.let { runCatching { nsd.unregisterServiceInfoCallback(it) } }
      if (found.remove(name) != null) emit()
    }
  }

  private fun emit() = sendEvent("onChange", mapOf("services" to found.values.toList()))
}
