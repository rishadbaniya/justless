// Valid Swift that an edit may write: SwiftUI, macros, concurrency and generics.
import SwiftUI
import Observation

@Observable
final class Model {
    var count = 0
    var name: String?
}

struct ContentView: View {
    @State private var model = Model()
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            if let name = model.name {
                Text("Hello, \(name)!")
            }
            ForEach(0..<model.count, id: \.self) { index in
                Text("\(index)")
            }
            Button("Add") { model.count += 1 }
                .buttonStyle(.borderedProminent)
        }
        .task { await refresh() }
    }

    func refresh() async {
        guard let name = model.name else { return }
        let label = if name.isEmpty { "none" } else { name }
        print(label)
    }
}

#Preview {
    ContentView()
}

actor Cache<Key: Hashable, Value> where Value: Sendable {
    private var storage: [Key: Value] = [:]

    func value(for key: Key, orInsert make: @Sendable () async throws -> Value) async rethrows -> Value {
        if let cached = storage[key] { return cached }
        let made = try await make()
        storage[key] = made
        return made
    }
}

enum Parse: Error { case bad(line: Int) }

func digits(in text: String) throws -> [Int] {
    let matches = text.matches(of: /\d+/)
    return try matches.map { match in
        guard let value = Int(match.output) else { throw Parse.bad(line: 0) }
        return value
    }
}

protocol Shape { var area: Double { get } }
func total(_ shapes: [any Shape]) -> Double { shapes.reduce(0) { $0 + $1.area } }
