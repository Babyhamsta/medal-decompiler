return function(subject)
    local all1, any1, mixed1, log1 = subject(true, false, 7)
    local all2, any2, mixed2, log2 = subject(true, true, 9)
    local all3, any3, mixed3, log3 = subject(false, nil, 3)
    return all1,
        any1,
        mixed1,
        log1,
        all2,
        any2,
        mixed2,
        log2,
        all3,
        any3,
        mixed3,
        log3
end
