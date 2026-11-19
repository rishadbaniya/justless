// Valid TypeScript that an edit may write: decorators, generics and type-level code.
import type { Request } from "express";
export type { Request };

declare module "express" {
  interface Request {
    user?: { id: string };
  }
}

export const enum Level {
  Low = 1,
  High = Low << 2,
}

type Getters<T> = {
  [K in keyof T as `get${Capitalize<string & K>}`]: () => T[K];
};
type Unwrap<T> = T extends Promise<infer U> ? Unwrap<U> : T;
interface Box<T> {
  value: T;
}
declare const tag: unique symbol;

function logged(target: unknown, context: ClassMethodDecoratorContext) {
  return target;
}

@sealed
export abstract class Repo<const T extends { id: string }> {
  @observable accessor count = 0;
  private items!: Map<string, T>;
  protected constructor(readonly name: string) {}
  abstract find(id: string): Promise<T | undefined>;

  @logged
  override toString(): string {
    return `${this.name}:${this.count}`;
  }

  assertDefined<V>(value: V): asserts value is NonNullable<V> {
    if (value == null) throw new Error("missing");
  }
}

export function overload(value: string): string;
export function overload(value: number): number;
export function overload(value: string | number) {
  return value;
}

const config = { port: 8080, host: "localhost" } as const satisfies Record<string, unknown>;
const port = <number>config.port;
namespace Shapes {
  export const unit = 1;
}

export async function main(make: new () => Repo<{ id: string }>) {
  await using handle = open();
  const repo = make();
  return repo?.find("a") ?? Shapes.unit + port;
}
