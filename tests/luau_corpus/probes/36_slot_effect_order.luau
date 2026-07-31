return function(subject)
    local a1, a2, a3 = subject(9)
    local b1, b2, b3 = subject(-4)
    return a1, a2, a3, b1, b2, b3
end
