-- Loyalty points math for the in-store kiosk game.
local M = {}

-- Points earned for a purchase amount in cents.
function M.points_for(cents)
  return math.floor(cents / 100)
end

-- Tier label for a lifetime points balance.
function M.tier_label(points)
  if M.points_for(points * 100) >= 500 then
    return "gold"
  end
  return "silver"
end

return M
