import Foundation

struct Point {
    var x: Int
    var y: Int
}

// One shopping cart.
class Cart {
    var items: [Int] = []

    func total(tax: Int) -> Int {
        return subtotal() + tax
    }

    private func subtotal() -> Int {
        return items.reduce(0, +)
    }
}

enum Color {
    case red
    case green
}

protocol Renderer {
    func render() -> String
}

extension Cart {
    func describe() -> String {
        return "cart with \(items.count)"
    }
}

func fetchTotal(cart: Cart) -> Int {
    return cart.total(tax: 5)
}
