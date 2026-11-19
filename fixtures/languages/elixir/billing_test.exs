defmodule Shop.BillingTest do
  use ExUnit.Case, async: true

  alias Shop.Billing

  describe "total/1" do
    test "totals items" do
      assert Billing.total(%Billing{items: [1, 2]}) == 6
    end
  end
end
