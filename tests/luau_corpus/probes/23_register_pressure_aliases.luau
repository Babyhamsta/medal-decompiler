return function(subject)
    local finishTruthy = subject(3, function(seed, a10, a20)
        return seed * 2, a10 + 1, a20 - 1
    end)
    local finishFalsy = subject(3, function(seed, a10, a20)
        return seed * 2, false, a20 - 1
    end)

    local mergedTruthy0, bucketTruthy, firstTruthy, secondTruthy, thirdTruthy =
        finishTruthy(0)
    local mergedTruthy100 = finishTruthy(100)

    local mergedFalsy0, bucketFalsy, firstFalsy, secondFalsy, thirdFalsy =
        finishFalsy(0)
    local mergedFalsy100 = finishFalsy(100)

    return mergedTruthy0,
        mergedTruthy100,
        bucketTruthy,
        firstTruthy,
        secondTruthy,
        thirdTruthy,
        mergedFalsy0,
        mergedFalsy100,
        bucketFalsy,
        firstFalsy,
        secondFalsy,
        thirdFalsy
end
