// An enum has runtime semantics of its own, which strip-only mode refuses.
enum Color {
  Red,
}
console.log(Color.Red);
