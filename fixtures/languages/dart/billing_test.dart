import 'package:test/test.dart';

import 'billing.dart';

void main() {
  test('totals items', () {
    final service = InvoiceService.standard();
    expect(service.total([1, 2]), 6);
  });

  test('applyTax', () {
    expect(applyTax(2), 4);
  });
}
