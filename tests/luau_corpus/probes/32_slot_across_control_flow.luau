return function(subject)
    local a1, a2, a3 = subject(8, true)
    local b1, b2, b3 = subject(8, false)
    return a1, a2, a3, b1, b2, b3
end
