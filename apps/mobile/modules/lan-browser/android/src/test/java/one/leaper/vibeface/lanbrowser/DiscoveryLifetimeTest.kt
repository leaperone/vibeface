package one.leaper.vibeface.lanbrowser

import org.junit.Test

class DiscoveryLifetimeTest {
  @Test
  fun lateCallbacksCannotReviveLostOrPreviousDiscoverySessions() {
    val state = DiscoveryLifetime()
    val first = state.start()
    val lost = checkNotNull(state.find(first, "node"))
    check(state.current(lost))
    check(state.lose(first, "node") == lost)
    check(!state.current(lost)) // A late legacy resolved callback cannot restore a lost service.
    val rediscovered = checkNotNull(state.find(first, "node"))
    check(state.current(rediscovered))
    check(!state.current(lost)) // Neither update nor lost from the old modern callback affects it.
    state.stop()
    check(!state.current(rediscovered))
    val second = state.start()
    val restarted = checkNotNull(state.find(second, "node"))
    check(state.find(first, "stale") == null)
    check(state.lose(first, "node") == null)
    check(!state.current(rediscovered))
    check(state.current(restarted))
    check(state.find(second, "node") == null) // Duplicate discovery cannot queue another resolve.
    println("DiscoveryLifetime callback sequence checks passed")
  }
}
