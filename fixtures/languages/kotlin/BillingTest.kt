package shop.billing

import org.junit.jupiter.api.Test

class BillingTest {
    @Test
    fun totalsItems() {
        val service = InvoiceService.create(MemoryLedger())
        service.total(listOf(1, 2))
    }
}
