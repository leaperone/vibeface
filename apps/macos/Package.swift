// swift-tools-version: 5.9
import PackageDescription

let package = Package(
  name: "VibefaceMac",
  platforms: [.macOS(.v14)],
  products: [
    .executable(name: "VibefaceMac", targets: ["VibefaceMac"]),
  ],
  targets: [
    .executableTarget(
      name: "VibefaceMac",
      path: "Sources/VibefaceMac",
      resources: [.process("Resources")]
    ),
  ]
)
