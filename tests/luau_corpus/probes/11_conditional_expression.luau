return function(subject)
    return subject(1, 5, 9),
        subject(7, 5, 9),
        subject(12, 5, 9),
        subject(-4, 5, 9),
        subject(9, 9, 5)
end
