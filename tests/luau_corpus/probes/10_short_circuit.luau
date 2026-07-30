return function(subject)
    local all, any, mixed, log = subject(true, false, 7)
    return all, any, mixed, log
end
