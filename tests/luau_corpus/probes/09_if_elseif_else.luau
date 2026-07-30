return function(subject)
    return subject(1, 10),
        subject(10, 10),
        subject(50, 10),
        subject(-5, 10),
        subject(0, 10),
        subject(9, 10)
end
