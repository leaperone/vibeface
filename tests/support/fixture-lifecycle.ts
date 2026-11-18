/** Run setup and use after acquiring fixture control, then stop it exactly once. */
export async function withControlledFixture({
  run,
  stop,
}: {
  run: () => Promise<void>;
  stop: () => Promise<unknown>;
}): Promise<void> {
  let primary: { error: unknown } | undefined;
  try {
    await run();
  } catch (error) {
    primary = { error };
    throw error;
  } finally {
    try {
      await stop();
    } catch (cleanup) {
      if (primary === undefined) throw cleanup;
      let message = 'Fixture failed and cleanup also failed';
      try {
        message = `${String(primary.error)}; cleanup also failed: ${String(cleanup)}`;
      } catch {
        // Preserve both thrown values even when their conversion to text fails.
      }
      throw new AggregateError([primary.error, cleanup], message);
    }
  }
}
