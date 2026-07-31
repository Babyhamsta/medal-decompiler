return function(subject)
    local a1, a2, a3 = subject(4, 1)
    local b1, b2, b3 = subject(4, 2)
    local c1, c2, c3 = subject(4, 3)
    return a1, a2, a3, b1, b2, b3, c1, c2, c3
end
