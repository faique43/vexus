# Gift card balance handling for the storefront.
module Store
  class GiftCard
    def initialize(balance_cents)
      @balance_cents = balance_cents
    end

    # Applies the card to a charge, returning the cents still owed.
    def apply_to(charge_cents)
      used = [@balance_cents, charge_cents].min
      deduct(used)
      charge_cents - used
    end

    def deduct(cents)
      @balance_cents -= cents
    end
  end
end
