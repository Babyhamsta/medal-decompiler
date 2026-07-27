local function wrap(tag, value)
    return { tag = tag, value = value }
end

local function choose(value: number, low: number, high: number)
    local band = if value < low then "low" elseif value > high then "high" else "middle"
    local signed = wrap("signed", if value < 0 then -value else value)
    return if band == "middle"
        then wrap("inside", signed.value)
        else wrap(band, if value == low or value == high then value * 2 else value)
end

return choose
