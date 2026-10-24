require "json"

package = JSON.parse(File.read(File.join(__dir__, "package.json")))

Pod::Spec.new do |s|
  s.name         = "SecureNative"
  s.version      = package["version"]
  s.summary      = package["description"]
  s.homepage     = package["homepage"]
  s.license      = package["license"]
  s.authors      = package["author"]

  s.platforms    = { :ios => min_ios_version_supported }
  s.source       = { :git => "https://github.com/leaperone/vibeface.git", :tag => "#{s.version}" }

  s.source_files = "ios/**/*.{h,m,mm,swift}", "ios/generated/**/*.{h,m,mm}", "cpp/**/*.{hpp,cpp,c,h}", "cpp/generated/**/*.{hpp,cpp,c,h}"
  s.vendored_frameworks = "build/VibefaceSecureFramework.xcframework"
  s.dependency    "uniffi-bindgen-react-native", "0.31.0-6"

  install_modules_dependencies(s)
end
