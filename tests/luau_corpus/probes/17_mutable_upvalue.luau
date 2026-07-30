return function(subject)
    local increment, decrement, read = subject(4)

    local afterIncrement1 = increment()
    local afterDecrement1 = decrement(2)
    local afterRead1 = read()
    local afterIncrement2 = increment(5)
    local afterRead2 = read()
    local afterDecrement2 = decrement()

    return afterIncrement1,
        afterDecrement1,
        afterRead1,
        afterIncrement2,
        afterRead2,
        afterDecrement2
end
