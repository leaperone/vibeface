export type ActionTransition<Scope> = { from: Scope; to: Scope }

export type ActionClaim<Scope> = {
  current(): boolean
  /** Move a claim across a transition made by that action, using the scope it held before the change. */
  rebind(transition: ActionTransition<Scope>): boolean
  finish(): boolean
}

/** One action slot. A superseded or cancelled claim cannot release a later claim, even in the same scope. */
export function createActionOwner<Scope>() {
  let active: ActionClaim<Scope> | null = null
  return {
    busy: () => active?.current() === true,
    claim(scope: Scope, isCurrent: (scope: Scope) => boolean): ActionClaim<Scope> | null {
      if (!isCurrent(scope) || active?.current()) return null
      const claim: ActionClaim<Scope> = {
        current: () => active === claim && isCurrent(scope),
        rebind({ from, to }) {
          if (active !== claim || !Object.is(scope, from) || !isCurrent(to)) return false
          scope = to
          return true
        },
        finish() {
          if (active !== claim) return false
          active = null
          return true
        },
      }
      active = claim
      return claim
    },
    cancel() { active = null },
  }
}
