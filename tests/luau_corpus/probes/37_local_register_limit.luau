return function(subject)
	local traceLow, countLow, carriedLow, capturedLow, ticksLow = subject(3)
	local traceHigh, countHigh, carriedHigh, capturedHigh, ticksHigh = subject(811)

	return traceLow,
		countLow,
		carriedLow,
		capturedLow,
		ticksLow,
		traceHigh,
		countHigh,
		carriedHigh,
		capturedHigh,
		ticksHigh
end
