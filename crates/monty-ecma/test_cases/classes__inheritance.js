// === Declarations, fields, methods, accessors and statics ===
class Point {
  static count = 0;
  tags = [];
  constructor(x, y) {
    this.x = x;
    this.y = y;
    Point.count++;
  }
  get length() {
    return Math.hypot(this.x, this.y);
  }
  set length(value) {
    const scale = value / this.length;
    this.x *= scale;
    this.y *= scale;
  }
  scale(factor) {
    return new Point(this.x * factor, this.y * factor);
  }
  static origin() {
    return new Point(0, 0);
  }
  toString() {
    return `(${this.x}, ${this.y})`;
  }
}
const p = new Point(3, 4);
console.log(p, p.length, String(p), p.scale(2), Point.origin(), Point.count, p.tags);
p.length = 10;
console.log(p.x, p.y, Object.keys(p), Object.getOwnPropertyNames(Point.prototype), Object.keys(Point));
console.log(typeof Point, Point.name, Point.length, Point.prototype.constructor === Point, p instanceof Point);
console.log(Object.getPrototypeOf(p) === Point.prototype, p.constructor === Point, p.hasOwnProperty('x'), 'length' in p);

// === Inheritance, super and new.target ===
class Animal {
  constructor(name) {
    this.name = name;
    this.kind = new.target.name;
  }
  speak() {
    return `${this.name} makes a sound`;
  }
  static create(name) {
    return new this(name);
  }
}
class Dog extends Animal {
  constructor(name) {
    super(name);
    this.tricks = [];
  }
  speak() {
    return super.speak() + ' (woof)';
  }
  static create(name) {
    const dog = super.create(name);
    dog.tricks.push('sit');
    return dog;
  }
}
class Puppy extends Dog {}
const puppy = new Puppy('Rex');
console.log(puppy, puppy.speak(), Dog.create('Fido'), Puppy.create('Bo').kind);
console.log(puppy instanceof Dog, puppy instanceof Animal, Object.getPrototypeOf(Puppy) === Dog, Puppy.name, Puppy.length);
console.log(Animal, Dog, Puppy, class {}, class extends Animal {}, (class Named {}).name);

// === Field initializers see this and the class; static blocks ===
class Config {
  static defaults = { verbose: false };
  static label = Config.name.toLowerCase();
  static {
    Config.ready = true;
    Config.initialized = Config.label + '!';
  }
  verbose = Config.defaults.verbose;
  doubled = this.compute();
  ['computed' + 1] = 'c';
  compute() {
    return 2;
  }
}
console.log(new Config(), Config.label, Config.ready, Config.initialized, Object.keys(Config));

// === Class expressions, computed and symbol members ===
const sym = Symbol('tagged');
const Expr = class Inner {
  static [sym]() {
    return 'symbol static';
  }
  [sym]() {
    return Inner.name;
  }
  static ['sta' + 'tic']() {
    return 'computed static';
  }
};
console.log(Expr.name, Expr[sym](), new Expr()[sym](), Expr.static(), typeof Inner);

// === Methods are not constructors; arrows keep this ===
class Counter {
  value = 0;
  increment = () => ++this.value;
  tick() {
    return [1, 2].map(() => this.value);
  }
}
const counter = new Counter();
const { increment } = counter;
increment();
increment();
console.log(counter.value, counter.tick());
try {
  new counter.tick();
} catch (err) {
  console.log(err.constructor.name, err.message);
}

// === Errors ===
const attempts = [
  () => Point(),
  () => {
    class Bad extends Animal {
      constructor() {
        this.x = 1;
      }
    }
    new Bad();
  },
  () => {
    class Bad extends Animal {
      constructor() {
        return 1;
      }
    }
    new Bad();
  },
  () => {
    class Bad extends Animal {
      constructor() {}
    }
    new Bad();
  },
  () => {
    class Bad extends Animal {
      constructor() {
        super('a');
        super('b');
      }
    }
    new Bad();
  },
  () => {
    class Bad extends 5 {}
  },
  () => {
    class Bad extends Animal {
      constructor() {
        return { replaced: true };
      }
    }
    console.log(new Bad());
  },
  () => {
    class Shape extends null {
      constructor() {
        return Object.create(Shape.prototype);
      }
    }
    console.log(Object.getPrototypeOf(Shape.prototype), new Shape() instanceof Shape);
  },
];
for (const attempt of attempts) {
  try {
    attempt();
    console.log('no error');
  } catch (err) {
    console.log(err.constructor.name + ': ' + err.message);
  }
}

// === Subclassing builtins ===
class MyError extends Error {
  constructor(message) {
    super(message);
    this.name = 'MyError';
  }
}
const myError = new MyError('custom');
console.log(myError instanceof MyError, myError instanceof Error, myError.message, myError.name, String(myError));
try {
  throw new MyError('thrown');
} catch (err) {
  console.log(err.constructor.name, err.message);
}

// === super in object literals ===
const base = {
  greet() {
    return 'base';
  },
};
const derived = {
  __proto__: base,
  greet() {
    return 'derived > ' + super.greet();
  },
};
console.log(derived.greet());
