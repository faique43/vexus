package shop

import kotlin.math.max

const val MAX_ITEMS = 100

// One shopping cart.
class Cart(val items: List<Int>) {
    fun total(tax: Int): Int = items.sum() + tax
}

object Registry {
    fun lookup(id: String): Int = id.length
}

interface Renderer {
    fun render(): String
}

enum class Color { RED, GREEN }

data class Line(val qty: Int)

fun describe(cart: Cart, verbose: Boolean): String {
    val n = max(cart.items.size, 1)
    return if (verbose) "cart(${Registry.lookup("x")}, $n)" else "cart"
}
