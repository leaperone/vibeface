package one.leaper.vibeface.lanbrowser

/** Callers serialize access with the browser lock. A new sighting cannot revive an old callback. */
internal class DiscoveryLifetime {
  data class Ticket(val session: Long, val serial: Long, val name: String)
  private var session = 0L
  private var serial = 0L
  private var active = false
  private val services = mutableMapOf<String, Ticket>()

  fun start(): Long { session++; active = true; services.clear(); return session }
  fun stop() { active = false; services.clear() }
  fun current(generation: Long) = active && generation == session
  fun current(ticket: Ticket) = current(ticket.session) && services[ticket.name] == ticket
  fun find(generation: Long, name: String): Ticket? {
    if (!current(generation) || services.containsKey(name)) return null
    return Ticket(session, ++serial, name).also { services[name] = it }
  }
  fun lose(generation: Long, name: String): Ticket? {
    if (!current(generation)) return null
    return services.remove(name)
  }
}
