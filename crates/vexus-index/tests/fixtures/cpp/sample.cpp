#include "cart.hpp"
#include <vector>

namespace shop {

// One shopping cart.
class Cart {
 public:
  int total(int tax) { return subtotal() + tax; }

 private:
  int subtotal() { return items_; }
  int items_ = 0;
};

struct Point {
  int x;
  int y;
};

enum class Color { Red, Green };

template <typename T>
T clamp_min(T value, T floor) {
  return value < floor ? floor : value;
}

int helper(int v);

}  // namespace shop

int shop::helper(int v) { return shop::clamp_min(v, 0); }

typedef int Cents;
using Money = int;
