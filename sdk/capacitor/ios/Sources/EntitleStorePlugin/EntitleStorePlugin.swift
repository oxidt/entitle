import Foundation
import Capacitor
import StoreKit

@objc(EntitleStorePlugin)
public class EntitleStorePlugin: CAPPlugin, CAPBridgedPlugin {
    public let identifier = "EntitleStorePlugin"
    public let jsName = "EntitleStore"
    public let pluginMethods: [CAPPluginMethod] = [
        CAPPluginMethod(name: "getProducts", returnType: CAPPluginReturnPromise),
        CAPPluginMethod(name: "purchase", returnType: CAPPluginReturnPromise),
        CAPPluginMethod(name: "finishTransaction", returnType: CAPPluginReturnPromise),
        CAPPluginMethod(name: "getCurrentPurchases", returnType: CAPPluginReturnPromise)
    ]
    private let store = EntitleStore()

    override public func load() {
        // Started at load so no update is missed; retained until JS adds a listener.
        store.startObservingUpdates { [weak self] transaction in
            self?.notifyListeners("purchaseUpdated", data: Self.purchaseJSON(transaction), retainUntilConsumed: true)
        }
    }

    @objc func getProducts(_ call: CAPPluginCall) {
        let ids = call.getArray("productIds", String.self) ?? []
        Task {
            do {
                let products = try await store.products(ids: ids)
                call.resolve(["products": products.map(Self.productJSON)])
            } catch {
                Self.reject(call, error)
            }
        }
    }

    @objc func purchase(_ call: CAPPluginCall) {
        guard let productId = call.getString("productId"), let accountToken = call.getString("accountToken") else {
            call.reject("productId and accountToken are required", "INVALID_ARGUMENT")
            return
        }
        Task {
            do {
                switch try await store.purchase(productId: productId, accountToken: accountToken) {
                case .purchased(let transactionId, let productId):
                    call.resolve([
                        "status": "purchased",
                        "purchase": ["store": "app_store", "transactionId": transactionId, "productId": productId]
                    ])
                case .cancelled:
                    call.resolve(["status": "cancelled"])
                case .pending:
                    call.resolve(["status": "pending"])
                }
            } catch {
                Self.reject(call, error)
            }
        }
    }

    @objc func finishTransaction(_ call: CAPPluginCall) {
        guard let transactionId = call.getString("transactionId") else {
            call.reject("transactionId is required", "INVALID_ARGUMENT")
            return
        }
        Task {
            await store.finish(transactionId: transactionId)
            call.resolve()
        }
    }

    @objc func getCurrentPurchases(_ call: CAPPluginCall) {
        Task {
            let transactions = await store.currentPurchases()
            call.resolve(["purchases": transactions.map(Self.purchaseJSON)])
        }
    }

    private static func purchaseJSON(_ transaction: Transaction) -> [String: Any] {
        ["store": "app_store", "transactionId": String(transaction.id), "productId": transaction.productID]
    }

    private static func productJSON(_ product: Product) -> [String: Any] {
        let currency = product.priceFormatStyle.currencyCode
        var json: [String: Any] = [
            "productId": product.id,
            "kind": product.subscription == nil ? "one_time" : "subscription",
            "title": product.displayName,
            "description": product.description,
            "price": [
                "formatted": product.displayPrice,
                "amountMinor": EntitleStore.amountMinor(product.price, currency: currency),
                "currency": currency
            ]
        ]
        if let subscription = product.subscription {
            json["subscriptionPeriod"] = EntitleStore.iso8601(subscription.subscriptionPeriod)
        }
        return json
    }

    private static func reject(_ call: CAPPluginCall, _ error: Error) {
        switch error {
        case EntitleStore.StoreError.productNotFound(let id):
            call.reject("product not found: \(id)", "PRODUCT_NOT_FOUND")
        case EntitleStore.StoreError.invalidAccountToken(let token):
            call.reject("accountToken is not a UUID: \(token)", "INVALID_ARGUMENT")
        case EntitleStore.StoreError.unverified(let cause):
            call.reject("transaction failed StoreKit verification", "UNVERIFIED", cause)
        default:
            call.reject(error.localizedDescription, "STORE_ERROR", error)
        }
    }
}
