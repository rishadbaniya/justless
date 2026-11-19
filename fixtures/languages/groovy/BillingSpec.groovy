package shop.billing

import spock.lang.Specification

class BillingSpec extends Specification {
    def "totals items"() {
        given:
        def service = InvoiceService.create(Mock(Ledger))

        expect:
        service.total([1L, 2L]) == 6
    }
}
