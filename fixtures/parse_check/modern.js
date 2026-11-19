#!/usr/bin/env node
// Valid JavaScript that an edit may write: JSX, classes, modules and recent syntax.
import data from "./data.json" with { type: "json" };
import * as path from "node:path";
export * as helpers from "./helpers.js";

const total = 1_000_000n;
const pattern = /[\p{L}\p{N}]+/gu;
const query = gql`
  query Items($first: Int) { items(first: $first) { id } }
`;

class Counter {
  #count = 0;
  static instances = 0;
  static {
    Counter.instances += 1;
  }
  get value() {
    return this.#count;
  }
  increment(by = 1) {
    this.#count += by;
    return #count in this;
  }
  async *ticks(limit) {
    for (let i = 0; i < limit; i++) yield i;
  }
}

export default function List({ items, onPick }) {
  const [first, ...rest] = items ?? [];
  const label = first?.name ?? "none";
  let cache;
  cache ??= new Map();
  cache ||= new Map();
  return (
    <>
      <h1 className="title">{`${label} and ${rest.length} more`}</h1>
      <ul>
        {items.map((item) => (
          <li key={item.id} onClick={() => onPick?.(item)}>
            {item.name}
          </li>
        ))}
      </ul>
    </>
  );
}

const result = await Promise.all([fetch(path.join("a", "b"))]);
for await (const chunk of stream()) {
  console.log(chunk, data, total, pattern, query, result);
}
outer: for (const row of [[1]]) {
  for (const cell of row) {
    if (cell) continue outer;
  }
}
module.exports = { Counter };
