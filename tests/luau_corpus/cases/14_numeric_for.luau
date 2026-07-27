local function ranges(size: number)
    local total = 0

    for index = 1, size do
        total += index
    end

    for index = size, 1, -2 do
        total -= index
    end

    for row = 1, 3 do
        for column = 2, 6, 2 do
            total += row * column
        end
    end

    return total
end

return ranges
