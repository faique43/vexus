import 'package:shop/pricing.dart';

class Cart {
  final List<int> items;

  Cart(this.items);

  // Total in cents.
  int total(int tax) {
    return subtotal() + tax;
  }

  int subtotal() => items.fold(0, (a, b) => a + b);
}

mixin Loggable {
  void log(String msg) {}
}

enum Color { red, green }

extension CartX on Cart {
  String describe() => 'cart';
}

int checkoutTotal(Cart cart, int tax) {
  return cart.total(tax);
}
