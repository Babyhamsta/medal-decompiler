local function search(groups, wanted)
    for groupIndex, group in ipairs(groups) do
        if group.disabled then
            continue
        end

        for itemIndex, item in ipairs(group.items) do
            if item == false then
                break
            end

            local function matches()
                if item == wanted then
                    return true, groupIndex, itemIndex
                end
                return false
            end

            local found, outer, inner = matches()
            if found then
                return outer, inner, item
            end
        end
    end

    return nil
end

return search
