local function makeCallbacks(prefix: string, initial: number)
    local total = initial

    local function update(label: string, ...)
        local count = select("#", ...)
        for _, value in ipairs({ ... }) do
            total += value
        end
        return prefix .. label, total, count
    end

    local function snapshot()
        return prefix, total
    end

    return update, snapshot
end

return makeCallbacks
