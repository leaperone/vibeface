export function parseFixtureReset(request: Record<string, unknown>) {
  const { op, kind } = request;
  if (op !== 'reset' || Object.keys(request).length !== 2 ||
      !Object.hasOwn(request, 'op') || !Object.hasOwn(request, 'kind')) {
    throw new Error('Fixture reset requires an operation and kind');
  }
  switch (kind) {
    case 'single':
    case 'delivery':
    case 'workspace':
    case 'mesh':
    case 'cursor':
      return { op, kind };
    default:
      throw new Error('Unknown fixture kind');
  }
}
