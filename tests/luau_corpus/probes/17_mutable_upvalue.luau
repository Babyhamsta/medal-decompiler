return function(subject)
    local counter = subject(4)
    return counter(), counter(), counter()
end
