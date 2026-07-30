return function(subject)
    local finish = subject(3, function(seed, a10, a20)
        return seed * 2, a10 + 1, a20 - 1
    end)
    return finish(0), finish(100)
end
