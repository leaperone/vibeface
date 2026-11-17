import { mobilePlatform } from '@/platform'
import { createWorkspaceHistoryCache } from './workspace-history-cache'

export const localWorkspaceHistory = createWorkspaceHistoryCache({ storage: mobilePlatform.storage })
