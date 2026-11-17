import type { ConfigContext, ExpoConfig } from 'expo/config'

export default ({ config }: ConfigContext): ExpoConfig => {
  const projectId = process.env.EXPO_PUBLIC_EAS_PROJECT_ID
  const googleServicesFile = process.env.GOOGLE_SERVICES_JSON
  return {
    ...config,
    name: config.name ?? 'Vibeface',
    slug: config.slug ?? 'vibeface',
    plugins: [...config.plugins ?? [], 'expo-notifications'],
    extra: { ...config.extra, ...(projectId ? { eas: { projectId } } : {}) },
    android: { ...config.android, ...(googleServicesFile ? { googleServicesFile } : {}) },
  }
}
