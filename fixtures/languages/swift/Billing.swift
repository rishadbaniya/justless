import Foundation

typealias Cents = Int

protocol Ledger {
    func record(_ amount: Cents)
}

struct Invoice {
    let id: String
    var amount: Cents
}

final class InvoiceService {
    private let ledger: Ledger

    init(ledger: Ledger) {
        self.ledger = ledger
    }

    func total(_ items: [Cents]) -> Cents {
        let sum = items.reduce(0, +)
        ledger.record(sum)
        return applyTax(sum)
    }
}

extension InvoiceService {
    func reset() {}
}

func applyTax(_ amount: Cents) -> Cents {
    return amount * 2
}
