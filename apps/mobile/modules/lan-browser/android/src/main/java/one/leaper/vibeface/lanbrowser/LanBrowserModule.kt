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
  private val waiting = ArrayDeque<NsdServiceInfo>()
  private var resolving = false
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
      val listener = object : NsdManager.DiscoveryListener {
        override fun onDiscoveryStarted(serviceType: String) {}
        override fun onDiscoveryStopped(serviceType: String) {}
        override fun onStartDiscoveryFailed(serviceType: String, errorCode: Int) {}
        override fun onStopDiscoveryFailed(serviceType: String, errorCode: Int) {}
        override fun onServiceFound(info: NsdServiceInfo) = resolve(info)
        override fun onServiceLost(info: NsdServiceInfo) = lost(info.serviceName)
      }
      discovery = listener
      nsd.discoverServices("_vibeface._udp", NsdManager.PROTOCOL_DNS_SD, listener)
    }
  }

  private fun stop() {
    synchronized(lock) {
      val listener = discovery ?: return
      discovery = null
      runCatching { nsd.stopServiceDiscovery(listener) }
      if (Build.VERSION.SDK_INT >= 34) callbacks.values.forEach { runCatching { nsd.unregisterServiceInfoCallback(it) } }
      callbacks.clear()
      waiting.clear()
      found.clear()
      multicast?.release()
      multicast = null
    }
  }

  private fun resolve(info: NsdServiceInfo) {
    synchronized(lock) {
      if (discovery == null) return
      if (Build.VERSION.SDK_INT < 34) {
        waiting.addLast(info)
        next()
        return
      }
      if (callbacks.containsKey(info.serviceName)) return
      val callback = object : NsdManager.ServiceInfoCallback {
        override fun onServiceInfoCallbackRegistrationFailed(errorCode: Int) {}
        override fun onServiceUpdated(service: NsdServiceInfo) = put(service.serviceName, service.attributes["t"], service.hostAddresses, service.port)
        override fun onServiceLost() = lost(info.serviceName)
        override fun onServiceInfoCallbackUnregistered() {}
      }
      callbacks[info.serviceName] = callback
      nsd.registerServiceInfoCallback(info, executor, callback)
    }
  }

  /** Runs under `lock`. */
  @Suppress("DEPRECATION")
  private fun next() {
    if (resolving || discovery == null) return
    val info = waiting.removeFirstOrNull() ?: return
    resolving = true
    nsd.resolveService(info, object : NsdManager.ResolveListener {
      override fun onResolveFailed(service: NsdServiceInfo, errorCode: Int) = done()
      override fun onServiceResolved(service: NsdServiceInfo) {
        put(service.serviceName, service.attributes["t"], listOfNotNull(service.host), service.port)
        done()
      }
    })
  }

  private fun done() {
    synchronized(lock) {
      resolving = false
      next()
    }
  }

  private fun put(name: String, tag: ByteArray?, hosts: List<InetAddress>, port: Int) {
    synchronized(lock) {
      val addrs = hosts.filterIsInstance<Inet4Address>().mapNotNull { it.hostAddress }.map { "$it:$port" }
      if (discovery == null || tag == null || addrs.isEmpty()) return
      found[name] = mapOf("tag" to String(tag, Charsets.UTF_8), "addrs" to addrs)
      emit()
    }
  }

  private fun lost(name: String) {
    synchronized(lock) {
      if (Build.VERSION.SDK_INT >= 34) callbacks.remove(name)?.let { runCatching { nsd.unregisterServiceInfoCallback(it) } }
      if (found.remove(name) != null) emit()
    }
  }

  private fun emit() = sendEvent("onChange", mapOf("services" to found.values.toList()))
}
