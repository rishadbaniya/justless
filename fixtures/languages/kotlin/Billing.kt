package shop.billing

import shop.core.Money

typealias Cents = Long

interface Ledger {
    fun record(amount: Cents)
}

class InvoiceService(private val ledger: Ledger) : Ledger by ledger {
    val currency = "EUR"

    fun total(items: List<Cents>): Cents {
        val sum = items.sum()
        ledger.record(sum)
        return applyTax(sum)
    }

    companion object {
        fun create(ledger: Ledger): InvoiceService = InvoiceService(ledger)
    }
}

fun applyTax(amount: Cents): Cents = amount * 2
