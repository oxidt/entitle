// swift-tools-version: 5.9
import PackageDescription

let package = Package(
    name: "OxidtEntitleCapacitor",
    platforms: [.iOS(.v15)],
    products: [
        .library(
            name: "OxidtEntitleCapacitor",
            targets: ["EntitleStorePlugin"])
    ],
    dependencies: [
        .package(url: "https://github.com/ionic-team/capacitor-swift-pm.git", from: "8.0.0")
    ],
    targets: [
        .target(
            name: "EntitleStorePlugin",
            dependencies: [
                .product(name: "Capacitor", package: "capacitor-swift-pm"),
                .product(name: "Cordova", package: "capacitor-swift-pm")
            ],
            path: "ios/Sources/EntitleStorePlugin"),
        .testTarget(
            name: "EntitleStorePluginTests",
            dependencies: ["EntitleStorePlugin"],
            path: "ios/Tests/EntitleStorePluginTests")
    ]
)
