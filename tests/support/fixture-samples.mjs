// Synthetic wire examples shared by the offline boundary and fake-child tests.
export const context = { artifacts: '/task', origin: 'http://127.0.0.1:8100' };
export const inspection = { wallet_files: 0, mesh_records: 1, chain_rpc_calls: 0 };
export function readyDetails(kind, { artifacts, origin } = context) {
  const run = artifacts + '/run';
  const roles = { mesh: ['public', 'member'], delivery: ['source', 'receiver', 'provider'] }[kind] ?? ['source'];
  const nodes = Object.fromEntries(roles.map((role, index) => {
    const url = index === 0 ? origin : 'http://127.0.0.1:' + (8200 + index);
    return [role, { url, entry: role === 'public' ? null : url + '/#bootstrap=abc',
      peer: String(index + 1).repeat(64), directory: run + '/' + role }];
  }));
  const gates = Object.fromEntries(['main', 'taskA', 'taskB', 'channel'].map(key => [key, run + '/gate-' + key]));
  return kind === 'workspace' ? { run, nodes, gates } : { run, nodes };
}
