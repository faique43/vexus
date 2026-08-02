defmodule Shop.Notifier.Dispatcher do
  @moduledoc "Routes order notifications to the right channel."

  # Sends one order notification through the requested channel.
  def dispatch(order_id, channel) do
    deliver(order_id, format_channel(channel))
  end

  defp format_channel(channel) do
    to_string(channel)
  end

  defp deliver(order_id, channel) do
    "order #{order_id} via #{channel}"
  end
end
