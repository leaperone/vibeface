import { useDevice } from '@vibeface/client/identity/context.tsx'
import { useGenerationAction } from '@vibeface/client/lib/use-generation-action.ts'

export function useRun({ formatError }: { formatError?: (cause: unknown) => string } = {}) {
  return useGenerationAction(useDevice().connectionGeneration, formatError)
}
