defmodule Shop.Billing do
  @moduledoc """
  Invoices with #{inspect(:interpolation)} in a heredoc.
  """
  use GenServer
  import Bitwise, only: [band: 2]
  alias Shop.{Invoice, Line}
  require Logger

  @type cents :: non_neg_integer()
  @rates %{standard: 20, reduced: 5}

  defstruct [:id, lines: [], status: :draft]

  defguard is_cents(value) when is_integer(value) and value >= 0

  @doc "Totals an invoice."
  @spec total(%__MODULE__{}) :: cents
  def total(%__MODULE__{lines: lines} = invoice) when is_list(lines) do
    lines
    |> Enum.map(&line_total/1)
    |> Enum.sum()
    |> then(&(&1 + tax(invoice, &1)))
  end

  defp line_total(%Line{qty: qty, price: price}), do: qty * price

  defp tax(%{status: status}, amount) do
    with {:ok, rate} <- Map.fetch(@rates, status),
         true <- is_cents(amount) do
      div(amount * rate, 100)
    else
      :error -> 0
      false -> raise ArgumentError, "bad amount: #{amount}"
    end
  end

  def parse(<<version::8, size::16, body::binary-size(size), _rest::binary>>) do
    %{version: version, body: body, flags: band(version, 0x0F)}
  end

  def words, do: ~w(net gross tax)a ++ [~r/\d+(\.\d+)?/i, ~c"chars", ~s(a "quoted" string)]

  def steps, do: for(n <- 1..10//2, rem(n, 3) != 0, into: %{}, do: {n, n * n})

  def classify(value) do
    cond do
      value in [:paid, :void] -> :closed
      match?({:ok, _}, value) -> :open
      true -> :unknown
    end
  end

  def update(invoice, id) do
    case invoice do
      %{id: ^id} = found -> %{found | status: :paid}
      _ -> invoice
    end
  end

  def safely(fun) do
    try do
      fun.()
    rescue
      error in [ArgumentError] -> {:error, Exception.message(error)}
    catch
      :exit, reason -> {:exit, reason}
    after
      Logger.debug(fn -> "done" end)
    end
  end

  @impl true
  def handle_call({:total, id}, _from, state) do
    receive do
      {:refresh, ^id} -> :ok
    after
      0 -> :ok
    end

    {:reply, Map.get(state, id), state}
  end

  defmacro audit(name, do: block) do
    quote do
      def unquote(name)(), do: unquote(block)
    end
  end

  def handlers do
    [fn
       {:ok, value} -> value
       {:error, _} = error -> error
     end, &Invoice.new/1, &String.upcase(&1)]
  end
end

defprotocol Shop.Printable do
  @fallback_to_any true
  def print(value)
end

defimpl Shop.Printable, for: Shop.Billing do
  def print(%{id: id}), do: "invoice #{id}"
end
