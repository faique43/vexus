require "json"
require_relative "helper"

module Shop
  # One shopping cart.
  class Cart
    def initialize(items)
      @items = items
    end

    # Total in cents.
    def total(tax)
      subtotal + tax
    end

    def subtotal
      @items.sum
    end

    def self.build(items)
      new(items)
    end
  end
end
