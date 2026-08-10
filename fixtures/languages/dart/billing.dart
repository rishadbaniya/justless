import 'package:shop/core.dart';

typedef Cents = int;

abstract class Ledger {
  void record(Cents amount);
}

class InvoiceService {
  final Ledger ledger;

  InvoiceService(this.ledger);

  InvoiceService.standard() : ledger = MemoryLedger();

  Cents total(List<Cents> items) {
    final sum = items.fold(0, (a, b) => a + b);
    ledger.record(sum);
    return applyTax(sum);
  }
}

Cents applyTax(Cents amount) {
  return amount * 2;
}
