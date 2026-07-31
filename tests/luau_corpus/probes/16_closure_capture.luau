return function(subject)
    local transform = subject(3, 5)
    local label1, addOffset1 = transform(1)
    local label2, addOffset2 = transform(2)
    local label3, addOffset3 = transform(10)
    return label1, addOffset1, label2, addOffset2, label3, addOffset3
end
