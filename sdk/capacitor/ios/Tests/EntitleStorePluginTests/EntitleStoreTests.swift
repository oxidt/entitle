import XCTest
@testable import EntitleStorePlugin

// StoreKit purchase flows need an app host for SKTestSession (a hostless SPM test bundle gets no
// products), so they are tested by hand with ios/StoreKit/Products.storekit. See the README.
final class EntitleStoreTests: XCTestCase {
    func testRejectsAccountTokenThatIsNoUUID() async {
        do {
            _ = try await EntitleStore().purchase(productId: "pro.monthly", accountToken: "user_42")
            XCTFail("expected invalidAccountToken")
        } catch EntitleStore.StoreError.invalidAccountToken {
        } catch {
            XCTFail("unexpected \(error)")
        }
    }

    func testAmountMinor() {
        XCTAssertEqual(EntitleStore.amountMinor(Decimal(string: "4.99")!, currency: "EUR"), 499)
        XCTAssertEqual(EntitleStore.amountMinor(Decimal(480), currency: "JPY"), 480)
        XCTAssertEqual(EntitleStore.amountMinor(Decimal(string: "1.234")!, currency: "KWD"), 1234)
    }
}
