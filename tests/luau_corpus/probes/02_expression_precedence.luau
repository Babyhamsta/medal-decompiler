return function(subject)
    local arithmetic1, powers1, text1, relation1, inverted1 = subject(2, 3, 4)
    local arithmetic2, powers2, text2, relation2, inverted2 = subject(-1, 5, 2)
    local arithmetic3, powers3, text3, relation3, inverted3 = subject(0, 0, 7)

    return arithmetic1,
        powers1,
        text1,
        relation1,
        inverted1,
        arithmetic2,
        powers2,
        text2,
        relation2,
        inverted2,
        arithmetic3,
        powers3,
        text3,
        relation3,
        inverted3
end
