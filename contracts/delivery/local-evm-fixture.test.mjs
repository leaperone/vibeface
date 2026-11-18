import assert from 'node:assert/strict';
import { test } from 'node:test';
import { deployFixtureContract, withLocalEvm } from './local-evm-fixture.mjs';

function fakes({ createChain, createProvider, startPolling, destroy, disconnect } = {}) {
  const calls = [];
  const rpc = {
    disconnect() {
      calls.push('disconnect');
      return disconnect?.();
    },
  };
  const ganache = {
    provider(options) {
      calls.push('chain');
      createChain?.(options);
      return rpc;
    },
  };
  class BrowserProvider {
    constructor(chain, network, options) {
      calls.push('provider');
      assert.equal(chain, rpc);
      assert.equal(network, undefined);
      assert.deepEqual(options, { cacheTimeout: -1 });
      createProvider?.();
    }

    set pollingInterval(interval) {
      calls.push('polling');
      assert.equal(interval, 10);
      startPolling?.();
    }

    destroy() {
      calls.push('destroy');
      return destroy?.();
    }
  }
  return { calls, rpc, options: { ganache, BrowserProvider } };
}

function fail(error) {
  throw error;
}

const unexpectedBody = () => assert.fail('fixture body ran during failed setup');

test('returns the body result and awaits cleanup in provider, chain order', async () => {
  const { calls, rpc, options } = fakes({
    createChain: options => assert.deepEqual(options, {
      logging: { quiet: true }, chain: { chainId: 31337, hardfork: 'shanghai' },
      wallet: { deterministic: true }, miner: { timestampIncrement: 0 },
    }),
    async destroy() {
      await Promise.resolve();
      calls.push('destroyed');
    },
    async disconnect() {
      await Promise.resolve();
      calls.push('disconnected');
    },
  });
  const ganacheOptions = { wallet: { deterministic: true }, miner: { timestampIncrement: 0 } };
  const result = await withLocalEvm({ ...options, ganacheOptions }, async fixture => {
    assert.equal(fixture.rpc, rpc);
    assert(fixture.provider instanceof options.BrowserProvider);
    await Promise.resolve();
    calls.push('body');
    return 42;
  });
  assert.equal(result, 42);
  assert.deepEqual(calls, ['chain', 'provider', 'polling', 'body', 'destroy', 'destroyed', 'disconnect', 'disconnected']);
});

test('chain creation failure owns no resources', async () => {
  const failure = new Error('chain');
  const { calls, options } = fakes({ createChain: () => fail(failure) });
  await assert.rejects(withLocalEvm(options, unexpectedBody), error => error === failure);
  assert.deepEqual(calls, ['chain']);
});

test('provider constructor failure still attempts chain cleanup', async () => {
  const failure = new Error('provider');
  const { calls, options } = fakes({
    createProvider: () => fail(failure),
    disconnect: () => Promise.reject(new Error('disconnect')),
  });
  await assert.rejects(withLocalEvm(options, unexpectedBody), error => error === failure);
  assert.deepEqual(calls, ['chain', 'provider', 'disconnect']);
});

test('polling setup failure cleans both resources', async () => {
  const failure = new Error('polling');
  const { calls, options } = fakes({ startPolling: () => fail(failure) });
  await assert.rejects(withLocalEvm(options, unexpectedBody), error => error === failure);
  assert.deepEqual(calls, ['chain', 'provider', 'polling', 'destroy', 'disconnect']);
});

for (const [name, failingStep] of [
  ['deployment returns the contract after waiting for it', null],
  ['factory deploy failure cleans both resources', 'deploy'],
  ['deployment wait failure cleans both resources', 'wait'],
]) {
  test(name, async () => {
    const failure = new Error('deployment failed');
    const { calls, options } = fakes();
    const artifact = { abi: [], bytecode: '0x00' };
    const signer = {};
    const args = ['image', { gasLimit: 29000000 }];
    const contract = {
      async waitForDeployment() {
        calls.push('wait');
        if (failingStep === 'wait') throw failure;
      },
    };
    class ContractFactory {
      constructor(abi, bytecode, runner) {
        calls.push('factory');
        assert.equal(abi, artifact.abi);
        assert.equal(bytecode, artifact.bytecode);
        assert.equal(runner, signer);
      }

      deploy(...values) {
        calls.push('deploy');
        assert.deepEqual(values, args);
        if (failingStep === 'deploy') throw failure;
        return Promise.resolve(contract);
      }
    }
    const result = withLocalEvm(options, () => deployFixtureContract(ContractFactory, artifact, signer, ...args));
    if (failingStep) await assert.rejects(result, error => error === failure);
    else assert.equal(await result, contract);
    const deployment = failingStep === 'deploy' ? ['factory', 'deploy'] : ['factory', 'deploy', 'wait'];
    assert.deepEqual(calls, ['chain', 'provider', 'polling', ...deployment, 'destroy', 'disconnect']);
  });
}

test('an async body failure remains the rejection when both cleanups fail', async () => {
  const failure = new Error('body');
  const { calls, options } = fakes({
    destroy: () => fail(new Error('destroy')),
    disconnect: () => Promise.reject(new Error('disconnect')),
  });
  await assert.rejects(withLocalEvm(options, () => Promise.reject(failure)), error => error === failure);
  assert.deepEqual(calls, ['chain', 'provider', 'polling', 'destroy', 'disconnect']);
});

test('a synchronous throw of undefined remains the primary failure', async () => {
  const { calls, options } = fakes({ destroy: () => fail(new Error('destroy')) });
  await assert.rejects(withLocalEvm(options, () => fail(undefined)), error => error === undefined);
  assert.deepEqual(calls, ['chain', 'provider', 'polling', 'destroy', 'disconnect']);
});

test('a synchronous destroy failure still disconnects the chain', async () => {
  const failure = new Error('destroy');
  const { calls, options } = fakes({ destroy: () => fail(failure) });
  await assert.rejects(withLocalEvm(options, () => {}), error => error === failure);
  assert.deepEqual(calls, ['chain', 'provider', 'polling', 'destroy', 'disconnect']);
});

test('a lone disconnect rejection of undefined is still a failure', async () => {
  const { calls, options } = fakes({ disconnect: () => Promise.reject(undefined) });
  await assert.rejects(withLocalEvm(options, () => {}), error => error === undefined);
  assert.deepEqual(calls, ['chain', 'provider', 'polling', 'destroy', 'disconnect']);
});

test('both cleanup failures are reported in order after a successful body', async () => {
  const destroyError = new Error('destroy');
  const disconnectError = new Error('disconnect');
  const { calls, options } = fakes({
    destroy: () => Promise.reject(destroyError),
    disconnect: () => fail(disconnectError),
  });
  await assert.rejects(withLocalEvm(options, () => {}), error => {
    assert(error instanceof AggregateError);
    assert.equal(error.errors.length, 2);
    assert.equal(error.errors[0], destroyError);
    assert.equal(error.errors[1], disconnectError);
    return true;
  });
  assert.deepEqual(calls, ['chain', 'provider', 'polling', 'destroy', 'disconnect']);
});
