<?php

declare(strict_types=1);

namespace Fixtures\ParseCheck;

use Attribute;
use Stringable;

/** Valid PHP that an edit may write: attributes, enums, match and readonly classes. */
#[Attribute(Attribute::TARGET_METHOD)]
final class Route
{
    public function __construct(public readonly string $path, public array $methods = ['GET'])
    {
    }
}

enum Suit: string
{
    case Hearts = 'H';
    case Spades = 'S';

    public function color(): string
    {
        return match ($this) {
            Suit::Hearts => 'Red',
            Suit::Spades => 'Black',
        };
    }
}

readonly class Point implements Stringable
{
    public function __construct(public int $x = 0, public int $y = 0)
    {
    }

    public function __toString(): string
    {
        return "{$this->x},{$this->y}";
    }
}

final class Controller
{
    #[Route('/posts/{id}', methods: ['GET'])]
    public function show(int|string $id, ?Point $at = null): never
    {
        $double = fn(int $n): int => $n * 2;
        $length = strlen(...);
        $name = $at?->__toString() ?? 'none';
        $sql = <<<SQL
            SELECT * FROM posts WHERE id = {$id}
            SQL;
        $raw = <<<'TEXT'
            $not_interpolated
            TEXT;
        echo $double(2), $length($name), $sql, $raw;
        exit(0);
    }
}
