local billing = require("billing")

describe("billing", function()
  it("totals items", function()
    assert.are.equal(6, billing.total({ 1, 2 }))
  end)
end)
