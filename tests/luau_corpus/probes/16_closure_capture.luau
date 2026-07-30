return function(subject)
    local transform = subject(3, 5)
    return transform(1), transform(2), transform(10)
end
