const path = require('path')
const { getDefaultConfig } = require('expo/metro-config')
const { withUniwindConfig } = require('uniwind/metro')

const config = getDefaultConfig(__dirname)
// Workspace packages list react as a peer, and pnpm can give them a newer patch than the app's; two copies crash the
// release bundle on the first hook, so every import of react resolves from the app.
const resolve = config.resolver.resolveRequest
config.resolver.resolveRequest = (context, name, platform) => {
  const own = name === 'react' || name.startsWith('react/') ? { ...context, originModulePath: path.join(__dirname, 'package.json') } : context
  return (resolve ?? context.resolveRequest)(own, name, platform)
}

module.exports = withUniwindConfig(config, {
  cssEntryFile: './global.css',
  dtsFile: './uniwind-types.d.ts',
})
