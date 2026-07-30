return function(subject)
    local a1, a2, a3, a4 = subject(3)
    local b1, b2, b3, b4 = subject(11)
    return a1, a2, a3, a4, b1, b2, b3, b4
end
