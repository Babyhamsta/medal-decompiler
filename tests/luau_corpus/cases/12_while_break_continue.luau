local function sumOdd(values, limit: number)
    local index = 0
    local total = 0

    while index < #values do
        index += 1
        local value = values[index]

        if value < 0 then
            continue
        end

        if value > limit then
            break
        end

        if value % 2 == 1 then
            total += value
        end
    end

    return total, index
end

return sumOdd
