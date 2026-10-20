// TypeScript in strip-only mode: everything here erases to the JavaScript Node
// runs with `--input-type=module-typescript`.

// === Annotations and generics ===
const count: number = 3;
let label: string | undefined;
const pairs: Array<[string, number]> = [['a', 1], ['b', 2]];
function identity<T>(value: T): T {
  return value;
}
const twice = <T,>(value: T): [T, T] => [value, value];
console.log(count, label, pairs, identity('x'), twice(4));
console.log(identity<number>(5), new Map<string, number>([['k', 1]]).get('k'));

// === Interfaces and type aliases ===
interface User {
  name: string;
  age?: number;
}
type Named = Pick<User, 'name'>;
type Handler = (user: User) => string;
const greet: Handler = (user) => `hi ${user.name}${user.age === undefined ? '' : ` (${user.age})`}`;
const users: User[] = [{ name: 'ada', age: 36 }, { name: 'lin' }];
console.log(users.map(greet));
const named: Named = { name: 'only' };
console.log(named);

// === as, satisfies, non-null, definite assignment ===
const widened = 'text' as string;
const config = { retries: 2 } satisfies { retries: number };
const maybe: { value?: number } = { value: 7 };
console.log(widened, config.retries, maybe.value!, (maybe as { value: number }).value + 1);
let assigned!: number;
console.log(typeof assigned);
const literal = 'fixed' as const;
console.log(literal);

// === Classes ===
abstract class Shape {
  abstract area(): number;
  describe(this: Shape): string {
    return `area ${this.area()}`;
  }
}
class Square extends Shape {
  side: number;
  readonly kind = 'square';
  private static count = 0;
  protected scale?: number;
  constructor(side: number) {
    super();
    this.side = side;
    Square.count += 1;
  }
  override area(): number {
    return this.side ** 2;
  }
  static made(): number {
    return Square.count;
  }
}
class Tagged<T> implements Iterable<T> {
  declare tag: string;
  constructor(private readonly items: T[]) {}
  [Symbol.iterator](): Iterator<T> {
    return this.items[Symbol.iterator]();
  }
}
const square = new Square(3);
console.log(square.describe(), square.kind, Square.made(), Object.keys(square));
console.log([...new Tagged(['p', 'q'])], Object.keys(new Tagged([])));

// === Overloads, declare, this parameters, optional parameters ===
function pick(value: number): number;
function pick(value: string): string;
function pick(value: number | string): number | string {
  return value;
}
declare const ambient: number;
declare function ambientFn(x: number): void;
declare class AmbientClass {}
function withThis(this: { base: number }, extra?: number, ...rest: number[]): number {
  return this.base + (extra ?? 0) + rest.length;
}
console.log(pick(1), pick('s'), typeof ambient, typeof ambientFn, typeof AmbientClass);
console.log(withThis.call({ base: 10 }), withThis.call({ base: 10 }, 5, 1, 2));

// === Type-only declarations vanish ===
import type { Nothing } from 'nowhere';
export type Exported = number;
declare module 'ambient-module' {
  export const inside: number;
}
declare global {
  interface Window {
    extra: number;
  }
}
declare namespace Ambient {
  const x: number;
}
namespace TypesOnly {
  export type Inner = string;
  export interface Shape {
    sides: number;
  }
}
declare enum AmbientEnum {
  A,
}
console.log(typeof Ambient, typeof TypesOnly, typeof AmbientEnum);

// === Enum-free alternatives still work ===
const Direction = { Up: 'up', Down: 'down' } as const;
type Direction = (typeof Direction)[keyof typeof Direction];
const move = (direction: Direction): string => `going ${direction}`;
console.log(move(Direction.Up), move('down'));

// === Async and generics together ===
async function fetchAll<T>(items: T[]): Promise<T[]> {
  return Promise.all(items.map(async (item) => item));
}
console.log(await fetchAll([1, 2, 3]));
function* numbers(): Generator<number, void, unknown> {
  yield 1;
  yield 2;
}
console.log([...numbers()]);

// === Angle brackets that are not assertions ===
const compared = count < 5 && count > 1;
const arrow = <A, B>(a: A, b: B): [A, B] => [a, b];
console.log(compared, arrow('a', 1));
