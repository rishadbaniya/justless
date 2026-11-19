package fixtures.parse_check;

import java.util.List;
import java.util.function.Function;

/** Valid Java that an edit may write: records, sealed types, patterns and text blocks. */
public sealed interface Modern permits Modern.Circle, Modern.Square, Modern.Other {
    record Circle(double radius) implements Modern {
        public Circle {
            if (radius < 0) throw new IllegalArgumentException("radius");
        }
    }

    record Square(double side) implements Modern {}

    non-sealed class Other implements Modern {}

    static double area(Modern shape) {
        return switch (shape) {
            case Circle(double r) when r > 10 -> Math.PI * r * r;
            case Circle c -> Math.PI * c.radius() * c.radius();
            case Square s -> {
                double side = s.side();
                yield side * side;
            }
            default -> 0;
        };
    }

    @SuppressWarnings("unchecked")
    static <T extends Comparable<? super T>> List<T> sorted(List<? extends T> items) {
        var copy = new java.util.ArrayList<T>(items);
        copy.sort(Comparable::compareTo);
        Function<T, String> show = item -> "%s".formatted(item);
        if (copy.getFirst() instanceof String text && !text.isBlank()) {
            System.out.println(show.apply(copy.getFirst()));
        }
        String json = """
            {"name": "modern", "kind": \"""text\"""}
            """;
        System.out.println(json);
        return copy;
    }

    @Override
    String toString();
}
