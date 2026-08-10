const std = @import("std");

pub const Invoice = struct {
    total: u64,

    pub fn add(self: *Invoice, amount: u64) void {
        self.total += applyTax(amount);
    }
};

fn applyTax(amount: u64) u64 {
    return amount * 2;
}

test "adds tax" {
    var invoice = Invoice{ .total = 0 };
    invoice.add(1);
    try std.testing.expectEqual(@as(u64, 2), invoice.total);
}

test applyTax {
    try std.testing.expectEqual(@as(u64, 4), applyTax(2));
}
