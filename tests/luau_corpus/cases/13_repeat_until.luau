local function converge(seed: number)
    local value = seed
    local iterations = 0

    repeat
        local previous = value
        value = math.floor((value + 10) / 2)
        iterations += 1
    until value == previous or iterations >= 20

    return value, iterations
end

return converge
