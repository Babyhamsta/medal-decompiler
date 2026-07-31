return function(subject)
    local update, snapshot = subject("id-", 10)
    local label, runningTotal, count = update("a", 1, 2, 3)
    local prefix, current = snapshot()
    return label, runningTotal, count, prefix, current
end
