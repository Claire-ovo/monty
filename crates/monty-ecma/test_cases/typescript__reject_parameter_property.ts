// `constructor(private x)` declares and assigns a field: emission, not erasure.
class Point {
  constructor(private x: number, public y: number) {}
}
console.log(new Point(1, 2));
