// Valid C# that an edit may write: records, primary constructors and patterns.
global using System.Text;

namespace Fixtures.ParseCheck;

using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading.Tasks;

public record Point(int X, int Y);

public readonly record struct Size(double Width, double Height);

[Obsolete("Use Repo<T> instead")]
public sealed class Repo<T>(string name) where T : class, new()
{
    private readonly List<T> _items = [];
    public required string Label { get; init; }
    public string Name => name;

    public async IAsyncEnumerable<T> StreamAsync()
    {
        foreach (var item in _items)
        {
            await Task.Yield();
            yield return item;
        }
    }

    public static string Describe(object shape) => shape switch
    {
        Point { X: > 0, Y: var y } p when y < 10 => $"point {p.X},{y}",
        Size(var w, var h) => $"size {w * h:F2}",
        string { Length: > 0 } text => text,
        null => "null",
        _ => "other",
    };

    public IEnumerable<string> Query(int[] numbers) =>
        from n in numbers
        where n % 2 == 0
        orderby n descending
        select $"{n}";

    public string Json() => $$"""
        { "name": "{{name}}", "count": {{_items.Count}} }
        """;
}

public static class Program
{
    public static async Task Main(string[] args)
    {
        var repo = new Repo<object>("demo") { Label = "x" };
        await foreach (var item in repo.StreamAsync()) Console.WriteLine(item);
        (int count, string text) pair = (args.Length, args.FirstOrDefault() ?? "");
        var moved = new Point(1, 2) with { Y = 3 };
        int Local(int v) => v * 2;
        Console.WriteLine($"{pair.count} {moved} {Local(2)} {Repo<object>.Describe(moved)}");
    }
}
