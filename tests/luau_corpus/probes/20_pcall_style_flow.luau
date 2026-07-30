return function(subject)
    local success = table.pack(subject(
        function()
            return 7, 8
        end,
        function()
            return 0
        end
    ))
    local recovered = table.pack(subject(
        function()
            return nil, "reason"
        end,
        function()
            return 9, 10
        end
    ))
    local failed = table.pack(subject(
        function()
            return nil, "reason"
        end,
        function()
            return nil, "still missing"
        end
    ))

    return success, recovered, failed
end
