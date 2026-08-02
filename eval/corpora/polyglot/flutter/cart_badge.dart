// Cart badge counter shown in the Flutter app bar.
class CartBadge {
  final List<int> quantities;

  CartBadge(this.quantities);

  // Number shown on the badge, capped at 99.
  int badgeCount() {
    return capAt(quantities.fold(0, (a, b) => a + b), 99);
  }

  int capAt(int value, int max) {
    return value > max ? max : value;
  }
}
