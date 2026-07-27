local function evaluate(a: number, b: number, c: number)
    local arithmetic = a + b * c - (a - b) / (c + 1)
    local powers = -(a ^ 2) + b ^ (c % 3)
    local text = "value:" .. tostring(a + b) .. ":" .. tostring(c)
    local relation = a < b and b <= c or a == c
    local inverted = not (a == b or c < a)
    return arithmetic, powers, text, relation, inverted
end

return evaluate
