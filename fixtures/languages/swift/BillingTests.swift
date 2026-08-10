import XCTest

final class BillingTests: XCTestCase {
    func testTotalsItems() {
        let service = InvoiceService(ledger: MemoryLedger())
        XCTAssertEqual(service.total([1, 2]), 6)
    }
}
