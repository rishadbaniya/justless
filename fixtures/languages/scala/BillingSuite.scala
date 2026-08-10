package shop.billing

import org.scalatest.funsuite.AnyFunSuite

class BillingSuite extends AnyFunSuite {
  test("totals items") {
    val service = InvoiceService.create(MemoryLedger())
    assert(service.total(List(1, 2)) == 6)
  }
}
