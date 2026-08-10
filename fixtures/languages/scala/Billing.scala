package shop.billing

import shop.core.Money

type Cents = Long

trait Ledger {
  def record(amount: Cents): Unit
}

case class Invoice(id: String, total: Cents)

class InvoiceService(ledger: Ledger) {
  private val currency = "EUR"

  def total(items: List[Cents]): Cents = {
    val sum = items.sum
    ledger.record(sum)
    applyTax(sum)
  }
}

object InvoiceService {
  def create(ledger: Ledger): InvoiceService = new InvoiceService(ledger)
}

def applyTax(amount: Cents): Cents = amount * 2
