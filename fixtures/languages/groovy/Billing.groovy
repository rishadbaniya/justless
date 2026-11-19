package shop.billing

import groovy.transform.CompileStatic
import shop.core.Money

interface Ledger {
    void record(long amount)
}

@CompileStatic
class InvoiceService {
    final Ledger ledger
    String currency = 'EUR'

    InvoiceService(Ledger ledger) {
        this.ledger = ledger
    }

    long total(List<Long> items) {
        long sum = items.sum() as long
        ledger.record(sum)
        return Taxes.applyTax(sum)
    }

    static InvoiceService create(Ledger ledger) {
        new InvoiceService(ledger)
    }
}

class Taxes {
    static long applyTax(long amount) {
        amount * 2
    }

    def describe(amount) {
        "${applyTax(amount)} cents"
    }
}
