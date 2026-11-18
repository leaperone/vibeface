export async function withLocalEvm({ ganache, BrowserProvider, ganacheOptions = {} }, run) {
  const rpc = ganache.provider({
    ...ganacheOptions,
    logging: { quiet: true },
    chain: { chainId: 31337, hardfork: 'shanghai' },
  });
  let provider;
  let completed = false;
  try {
    provider = new BrowserProvider(rpc, undefined, { cacheTimeout: -1 });
    provider.pollingInterval = 10;
    const result = await run({ rpc, provider });
    completed = true;
    return result;
  } finally {
    const cleanupErrors = [];
    try {
      await provider?.destroy();
    } catch (error) {
      cleanupErrors.push(error);
    }
    try {
      await rpc.disconnect();
    } catch (error) {
      cleanupErrors.push(error);
    }
    if (completed) {
      if (cleanupErrors.length === 1) throw cleanupErrors[0];
      if (cleanupErrors.length > 1) throw new AggregateError(cleanupErrors, 'Local EVM cleanup failed');
    }
  }
}

export async function deployFixtureContract(ContractFactory, { abi, bytecode }, signer, ...args) {
  const contract = await new ContractFactory(abi, bytecode, signer).deploy(...args);
  await contract.waitForDeployment();
  return contract;
}
