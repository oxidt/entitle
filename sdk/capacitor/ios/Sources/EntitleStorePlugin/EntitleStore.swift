import Foundation
import StoreKit

/// StoreKit 2 flows for entitle. Never finishes a transaction on its own: the app syncs it with
/// entitle first and then calls `finish(transactionId:)`.
public final class EntitleStore {
    public enum StoreError: Error {
        case productNotFound(String)
        case invalidAccountToken(String)
        case unverified(Error)
    }

    public enum PurchaseOutcome: Equatable {
        case purchased(transactionId: String, productId: String)
        case cancelled
        case pending
    }

    private var updatesTask: Task<Void, Never>?

    public init() {}

    deinit {
        updatesTask?.cancel()
    }

    /// Listens to `Transaction.updates`: renewals, Ask to Buy, other devices, refunds, and
    /// unfinished transactions at launch. Only verified transactions are reported.
    public func startObservingUpdates(_ onUpdate: @escaping @Sendable (Transaction) -> Void) {
        updatesTask?.cancel()
        updatesTask = Task.detached {
            for await result in Transaction.updates {
                if case .verified(let transaction) = result {
                    onUpdate(transaction)
                }
            }
        }
    }

    public func products(ids: [String]) async throws -> [Product] {
        try await Product.products(for: ids)
    }

    public func purchase(productId: String, accountToken: String) async throws -> PurchaseOutcome {
        guard let token = UUID(uuidString: accountToken) else {
            throw StoreError.invalidAccountToken(accountToken)
        }
        guard let product = try await Product.products(for: [productId]).first else {
            throw StoreError.productNotFound(productId)
        }
        switch try await product.purchase(options: [.appAccountToken(token)]) {
        case .success(let verification):
            switch verification {
            case .verified(let transaction):
                return .purchased(transactionId: String(transaction.id), productId: transaction.productID)
            case .unverified(_, let error):
                throw StoreError.unverified(error)
            }
        case .userCancelled:
            return .cancelled
        case .pending:
            return .pending
        @unknown default:
            return .cancelled
        }
    }

    /// Finishes the unfinished transaction with this id; already finished or unknown ids are a no-op.
    public func finish(transactionId: String) async {
        for await result in Transaction.unfinished {
            let transaction = result.unsafePayloadValue
            if String(transaction.id) == transactionId {
                await transaction.finish()
                return
            }
        }
    }

    /// Active subscriptions and non-consumables (`Transaction.currentEntitlements`), verified only.
    public func currentPurchases() async -> [Transaction] {
        var transactions: [Transaction] = []
        for await result in Transaction.currentEntitlements {
            if case .verified(let transaction) = result {
                transactions.append(transaction)
            }
        }
        return transactions
    }

    /// Price in the currency's minor unit, e.g. 4.99 EUR → 499, 480 JPY → 480.
    static func amountMinor(_ price: Decimal, currency: String) -> Int {
        let formatter = NumberFormatter()
        formatter.numberStyle = .currency
        formatter.currencyCode = currency
        var scaled = price * pow(10, formatter.maximumFractionDigits)
        var rounded = Decimal()
        NSDecimalRound(&rounded, &scaled, 0, .plain)
        return NSDecimalNumber(decimal: rounded).intValue
    }

    /// ISO 8601 duration, e.g. `P1M`.
    static func iso8601(_ period: Product.SubscriptionPeriod) -> String {
        let unit: String
        switch period.unit {
        case .day: unit = "D"
        case .week: unit = "W"
        case .month: unit = "M"
        case .year: unit = "Y"
        @unknown default: unit = "D"
        }
        return "P\(period.value)\(unit)"
    }
}
