return function(subject)
    local totalNoBreak, indexNoBreak = subject({ 1, -2, 3, 4, -5, 7, 9 }, 12)
    local totalBreak, indexBreak = subject({ 1, -2, 3, 4, -5, 7, 9 }, 5)
    return totalNoBreak, indexNoBreak, totalBreak, indexBreak
end
