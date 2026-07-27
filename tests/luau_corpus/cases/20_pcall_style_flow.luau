local function attempt(callback, ...)
    local values = { callback(...) }
    if values[1] == nil then
        return false, values[2] or "missing value"
    end
    return true, table.unpack(values)
end

local function run(callback, fallback, ...)
    local ok, value, extra = attempt(callback, ...)

    if not ok then
        local recovered, replacement = attempt(fallback, value)
        if not recovered then
            return nil, replacement
        end
        return replacement, "recovered"
    end

    return value, extra
end

return run
