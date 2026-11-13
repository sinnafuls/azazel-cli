-- A virtual model. Choose `virtual/auto` in `/model` (or `--model virtual/auto`) and every
-- request goes to the model this function picks for it: questions that need thought to the
-- `slow` role, everything else to the `smol` role.
--
-- Copy to ~/.azazel/extensions/auto_model.luau.

local HARD = { "why", "design", "architecture", "debug", "investigate", "review" }

azazel.router("auto", function(request)
	-- The requests after tool results, and retries, stay on the model that began the turn: its
	-- prompt cache is warm, and a turn that changes models half-way pays for the context twice.
	if request.reason ~= "user" and request.previous then
		return request.previous
	end
	local text = string.lower(request.text or "")
	for _, word in ipairs(HARD) do
		if string.find(text, word, 1, true) then
			return "@slow"
		end
	end
	return "@smol"
end)
