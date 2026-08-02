local json = require("json")

local M = {}

-- Formats cents as dollars.
function M.format_cents(cents)
  return "$" .. tostring(cents / 100)
end

-- Renders one receipt line using the formatter.
function M.receipt_line(label, cents)
  return label .. " " .. M.format_cents(cents)
end

function M:describe()
  return M.receipt_line("total", 100)
end

local function clamp(x, floor)
  if x < floor then return floor end
  return x
end

function standalone(a, b)
  return clamp(a, 0) + b
end

-- Assigned function values index like `function M.name(...)` does.
M.round = function(x)
  return math.floor(x + 0.5)
end

local double = function(x)
  return x * 2
end

M.helpers = {
  triple = function(x)
    return double(x) * 3
  end,
}

return M
