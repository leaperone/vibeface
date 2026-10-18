import assert from "node:assert/strict"
import test from "node:test"
import { orbStateFor, thinkingOrbStates, type OrbPlace } from "./orb-state.ts"

const places: OrbPlace[] = [
  "connection", "wait", "refresh", "reconcile", "idle-chat", "idle-link", "trace", "reply", "plan",
]

test("every thinking orb state is used by one place", () => {
  const used = places.map((place) => orbStateFor(place))
  assert.deepEqual([...used].sort(), [...thinkingOrbStates].sort())
  assert.equal(new Set(used).size, 9)
})
