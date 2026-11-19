//! Valid Zig that an edit may write: comptime, error unions and labeled blocks.
const std = @import("std");

pub const Shape = union(enum) {
    circle: f64,
    square: f64,

    pub fn area(self: Shape) f64 {
        return switch (self) {
            .circle => |r| std.math.pi * r * r,
            .square => |s| s * s,
        };
    }
};

const ParseError = error{ Empty, TooLong };

fn first(comptime T: type, items: []const T) ParseError!T {
    if (items.len == 0) return error.Empty;
    return items[0];
}

pub fn main() !void {
    var gpa = std.heap.GeneralPurposeAllocator(.{}){};
    defer _ = gpa.deinit();
    const allocator = gpa.allocator();
    var list = std.ArrayList(u8).init(allocator);
    errdefer list.deinit();
    const value = first(u8, &[_]u8{ 1, 2 }) catch |err| switch (err) {
        error.Empty => 0,
        else => return err,
    };
    const total = blk: {
        var sum: u32 = 0;
        for (list.items, 0..) |item, i| sum += item + @as(u32, @intCast(i));
        break :blk sum;
    };
    const maybe: ?u8 = null;
    std.debug.print("{d} {d} {d}\n", .{ value, total, maybe orelse 0 });
}

test "first returns the head" {
    try std.testing.expectEqual(@as(u8, 1), try first(u8, &[_]u8{1}));
}
