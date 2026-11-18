import type { Caller, FixtureRequest, NativeRequest, ResultByOp, WebRequest } from './fixture-contract';

declare const web: Caller<WebRequest<'cursor'>, 'cursor'>;
declare const delivery: Caller<NativeRequest<'delivery'>, 'delivery'>;
declare const workspace: Caller<NativeRequest<'workspace'>, 'workspace'>;

// This file is checked with the consumer project and never executed.
const cursor: Promise<ResultByOp<'cursor'>['cursor_inspect']> = web({ op: 'cursor_inspect' });
const setup = workspace({ op: 'workspace_native_setup', invitation_file: '/task/invite' });
setup.then(result => result.gates.taskA);

// @ts-expect-error Native operations cannot cross the Web control boundary.
web({ op: 'native_join' });
// @ts-expect-error Cursor's topology contains only source.
web({ op: 'restart', role: 'receiver' });
// @ts-expect-error This scoped Web client only resets its own fixture kind.
web({ op: 'reset', kind: 'delivery' });
// @ts-expect-error Native delivery cannot release a workspace gate.
delivery({ op: 'workspace_release', gate: 'main' });
// @ts-expect-error Native workspace cannot invoke the delivery provider.
workspace({ op: 'native_provider_stage', stage: 'ready' });
// @ts-expect-error Malformed producer examples must use the explicit rejection helper.
delivery({ op: 'native_provider_stage', stage: 'ready', role: 'source' });
// @ts-expect-error Missing gate.
workspace({ op: 'workspace_release' });
// @ts-expect-error This operation has no mesh pairing result.
cursor.then(result => result.peer);

// @ts-expect-error Every newly admitted request operation needs a result decoder.
type MissingDecoder = ResultByOp<'single'>[(FixtureRequest | { op: 'future_operation' })['op']];
