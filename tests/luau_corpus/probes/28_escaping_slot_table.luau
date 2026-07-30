return function(subject)
    local a1, a2 = subject(5)
    local b1, b2 = subject(-2)
    return a1, a2, b1, b2
end
