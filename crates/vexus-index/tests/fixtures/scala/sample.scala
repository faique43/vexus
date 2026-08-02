package shop

import shop.util.Slug

object Registry {
  def lookup(id: String): Int = id.length
}

// One shopping cart.
class Cart(items: List[Int]) {
  def total(tax: Int): Int = subtotal() + tax
  private def subtotal(): Int = items.sum
}

trait Renderer {
  def render(): String
}

case class Line(qty: Int)

enum Color {
  case Red, Green
}
