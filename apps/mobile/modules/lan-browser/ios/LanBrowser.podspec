Pod::Spec.new do |s|
  s.name           = 'LanBrowser'
  s.version        = '1.0.0'
  s.summary        = 'Finds Vibeface Nodes on the LAN over Bonjour'
  s.description    = 'Finds Vibeface Nodes on the LAN over Bonjour'
  s.author         = 'LEAPERone'
  s.homepage       = 'https://github.com/leaperone/vibeface'
  s.license        = 'UNLICENSED'
  s.platforms      = { :ios => '16.4' }
  s.swift_version  = '5.9'
  s.source         = { git: '' }
  s.static_framework = true

  s.dependency 'ExpoModulesCore'

  s.source_files = '**/*.swift'
  s.pod_target_xcconfig = { 'DEFINES_MODULE' => 'YES' }
end
