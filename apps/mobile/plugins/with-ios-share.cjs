const fs = require('node:fs')
const path = require('node:path')
const { createRequire } = require('node:module')
const { withXcodeProject } = require('expo/config-plugins')
const sharingRequire = createRequire(require.resolve('expo-sharing/package.json'))
const plist = sharingRequire('@expo/plist').default

module.exports = (config) => withXcodeProject(config, (mod) => {
  const project = mod.modResults
  const target = 'expo-sharing-extension'
  if (!project.pbxTargetByName(target) || !project.pbxTargetByName('Vibeface')) {
    throw new Error('The iOS app and sharing extension targets are required')
  }
  const version = mod.version
  const buildNumber = mod.ios?.buildNumber
  if (typeof version !== 'string' || !/^[0-9]+\.[0-9]+\.[0-9]+$/.test(version)
    || typeof buildNumber !== 'string' || !/^[1-9][0-9]{0,3}(?:\.[0-9]{1,2}){0,2}$/.test(buildNumber)) {
    throw new Error('The sharing extension requires valid app version and iOS build number metadata')
  }
  project.updateBuildProperty('MARKETING_VERSION', JSON.stringify(version), undefined, target)
  project.updateBuildProperty('CURRENT_PROJECT_VERSION', JSON.stringify(buildNumber), undefined, target)
  const infoPath = path.join(mod.modRequest.platformProjectRoot, target, 'Info.plist')
  const info = plist.parse(fs.readFileSync(infoPath, 'utf8'))
  info.CFBundleDisplayName = 'Vibeface'
  fs.writeFileSync(infoPath, plist.build(info))
  project.updateBuildProperty('INFOPLIST_KEY_CFBundleDisplayName', '"Vibeface"', undefined, target)

  const values = ['IOS_SIGNING_TEAM', 'IOS_SIGNING_PROFILE_UUID', 'IOS_SHARE_SIGNING_PROFILE_UUID', 'IOS_SIGNING_IDENTITY']
    .map((key) => process.env[key] ?? '')
  if (values.some(Boolean)) {
    const [team, appProfile, shareProfile, identity] = values
    const uuid = /^[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}$/i
    if (!/^[A-Z0-9]{10}$/.test(team) || !uuid.test(appProfile) || !uuid.test(shareProfile)
      || appProfile.toUpperCase() === shareProfile.toUpperCase() || !/^[0-9a-f]{40}$/i.test(identity)) {
      throw new Error('The iOS targets require a complete, distinct manual signing configuration')
    }
    for (const [targetName, bundleId, profile] of [
      ['Vibeface', 'one.leaper.vibeface', appProfile],
      [target, 'one.leaper.vibeface.share', shareProfile],
    ]) {
      const nativeTarget = project.pbxTargetByName(targetName)
      const configurations = project.pbxXCConfigurationList()[nativeTarget.buildConfigurationList].buildConfigurations
        .map(({ value }) => project.pbxXCBuildConfigurationSection()[value])
      if (!configurations.some((entry) => entry.name === 'Release')
        || configurations.some((entry) => entry.buildSettings.PRODUCT_BUNDLE_IDENTIFIER.replaceAll('"', '') !== bundleId)) {
        throw new Error('The iOS signing target does not match its expected bundle identity')
      }
      for (const [key, value] of Object.entries({
        CODE_SIGN_STYLE: 'Manual', DEVELOPMENT_TEAM: team, CODE_SIGN_IDENTITY: identity,
        PROVISIONING_PROFILE: profile, PROVISIONING_PROFILE_SPECIFIER: profile,
      })) project.updateBuildProperty(key, value, 'Release', targetName)
    }
  }
  return mod
})
