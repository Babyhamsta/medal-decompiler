local function mark(log, label, value)
    log[#log + 1] = label
    return value
end

local function run(first, second, third)
    local log = {}
    local all = mark(log, "a", first) and mark(log, "b", second) and mark(log, "c", third)
    local any = mark(log, "x", first) or mark(log, "y", second) or mark(log, "z", third)
    local mixed = (first and mark(log, "left", second)) or mark(log, "fallback", third)
    return all, any, mixed, log
end

return run
