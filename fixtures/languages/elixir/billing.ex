defmodule Shop.Billing do
  @moduledoc "Invoice totals."

  alias Shop.Ledger
  import Shop.Money, only: [round_cents: 1]

  defstruct [:id, items: []]

  @type cents :: non_neg_integer()

  @spec total(%__MODULE__{}) :: cents
  def total(%__MODULE__{items: items}) do
    items
    |> Enum.sum()
    |> apply_tax()
    |> round_cents()
  end

  def record(invoice, ledger \\ Ledger) do
    ledger.record(total(invoice))
  end

  defp apply_tax(amount) when is_integer(amount), do: amount * 2

  defmacro cents(value), do: value
end

defprotocol Shop.Printable do
  def print(document)
end
