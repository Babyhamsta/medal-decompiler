local function classify(value: number, limit: number)
    local label

    if value < 0 then
        label = "negative"
    elseif value == 0 then
        label = "zero"
    elseif value < limit then
        local distance = limit - value
        if distance == 1 then
            label = "almost"
        else
            label = "inside"
        end
    else
        label = "outside"
    end

    return label
end

return classify
