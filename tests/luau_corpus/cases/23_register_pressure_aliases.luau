local function pressure(seed: number, source)
    local a01 = seed + 1
    local a02 = seed + 2
    local a03 = seed + 3
    local a04 = seed + 4
    local a05 = seed + 5
    local a06 = seed + 6
    local a07 = seed + 7
    local a08 = seed + 8
    local a09 = seed + 9
    local a10 = seed + 10
    local a11 = seed + 11
    local a12 = seed + 12
    local a13 = seed + 13
    local a14 = seed + 14
    local a15 = seed + 15
    local a16 = seed + 16
    local a17 = seed + 17
    local a18 = seed + 18
    local a19 = seed + 19
    local a20 = seed + 20
    local a21 = seed + 21
    local a22 = seed + 22
    local a23 = seed + 23
    local a24 = seed + 24
    local a25 = seed + 25
    local a26 = seed + 26
    local a27 = seed + 27
    local a28 = seed + 28
    local a29 = seed + 29
    local a30 = seed + 30

    local first, second, third = source(seed, a10, a20)
    local aliasOne = first
    local aliasTwo = aliasOne
    local merged

    if second then
        merged = aliasTwo + a01 + a03 + a05 + a07 + a09 + a11 + a13 + a15
    else
        merged = third + a02 + a04 + a06 + a08 + a10 + a12 + a14 + a16
    end

    local bucket = {
        a17,
        a18,
        a19,
        a20,
        a21,
        a22,
        a23,
        a24,
        a25,
        a26,
        a27,
        a28,
        a29,
        a30,
    }

    local function finish(extra)
        return merged + extra, bucket, first, second, third
    end

    return finish
end

return pressure
