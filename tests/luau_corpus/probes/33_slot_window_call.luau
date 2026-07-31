return function(subject)
    local a1, a2 = subject(3)
    local b1, b2 = subject(-11)
    local c1, c2 = subject(0)
    return a1, a2, b1, b2, c1, c2
end
