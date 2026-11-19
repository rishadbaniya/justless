// Valid Dart that an edit may write: null safety, records, patterns and class modifiers.
import 'dart:async';

sealed class Shape {}

final class Circle extends Shape {
  Circle(this.radius);
  final double radius;
}

base class Square extends Shape {
  Square({required this.side});
  final double side;
}

interface class Named {
  String get name => 'named';
}

mixin Logging {
  void log(String message) => print('[${runtimeType}] $message');
}

extension on String {
  String get shout => '${toUpperCase()}!';
}

extension type UserId(int value) {}

double area(Shape shape) => switch (shape) {
      Circle(radius: var r) when r > 0 => 3.14 * r * r,
      Circle() => 0,
      Square(:final side) => side * side,
    };

(int, String) pair() => (1, 'one');

class Repo with Logging {
  late final Map<String, int> cache = {};
  int? maybe;

  Future<List<int>> load(List<int> ids) async {
    final (count, label) = pair();
    final items = [
      for (final id in ids)
        if (id > count) id,
      ...?maybe == null ? null : [maybe!],
    ];
    log('$label ${items.length}'.shout);
    return items..sort();
  }

  Stream<int> ticks(int limit) async* {
    for (var i = 0; i < limit; i++) {
      yield i;
    }
  }
}

void main() async {
  final repo = Repo()..maybe = 3;
  await for (final tick in repo.ticks(2)) {
    print(tick);
  }
  if (repo.maybe case final value?) print(value);
}
