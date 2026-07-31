return function(subject)
    local a1, a2 = subject(9)
    local b1, b2 = subject(-4)
    local c1, c2 = subject(0)
    return a1, a2, b1, b2, c1, c2
end
