defmodule Shop.Cart do
  @moduledoc "One shopping cart."

  import Enum
  alias Shop.Pricing

  def total(items, tax) do
    subtotal(items) + tax
  end

  defp subtotal(items) do
    Enum.sum(items)
  end

  def empty do
    []
  end
end
