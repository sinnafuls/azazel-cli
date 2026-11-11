-- guard.luau: refuses shell commands that match a dangerous pattern.
--
-- Install: copy this file to ~/.azazel/extensions/ (every project) or to
-- <project>/.azazel/extensions/ (one project, once you have trusted it), then run /reload.
-- /ext lists the loaded scripts and shows the error of any script that failed.

-- Lua patterns, tried against the command line followed by one space.
local BLOCKED = {
	-- rm with the filesystem root as its target: `rm -rf /`, `rm -fr /*`. `%f[%a]` makes the
	-- match start at a word, so `farm -x /` does not count.
	{ pattern = "%f[%a]rm%s+%-%a+%s+/%*?[%s;&|]", why = "it would delete the whole filesystem" },
	-- writing straight to a disk
	{ pattern = "dd%s+.*of=/dev/[sh]d", why = "it would overwrite a disk" },
	{ pattern = "mkfs%.?%w*%s+/dev/", why = "it would format a disk" },
	-- the fork bomb `:(){ :|:& };:`
	{ pattern = ":%(%)%s*{.*};%s*:", why = "it is a fork bomb" },
}

-- azazel.on(event, handler) runs `handler` every time `event` happens. It can only be called
-- here, while the script loads. `tool_call` fires before a tool runs, for every agent:
--   ev.tool   the tool's name
--   ev.input  its arguments, as the model wrote them
--   ev.agent  who is calling: { id, def, parent, depth, session_id }
-- `ctx` describes the session: ctx.cwd, ctx.session_id, ctx.agent_id and ctx.mode.
azazel.on("tool_call", function(ev, ctx)
	-- A handler that raises an error disables its script, so check what you index.
	local command = type(ev.input) == "table" and ev.input.command
	if ev.tool ~= "shell" or type(command) ~= "string" then
		-- Returning nothing lets the call go ahead unchanged.
		return nil
	end
	local line = command .. " "
	for _, rule in ipairs(BLOCKED) do
		if string.find(line, rule.pattern) then
			-- azazel.state is a small key-value store that survives restarts and /reload. It is
			-- kept per script, in ~/.azazel/extensions/.state/guard.json.
			azazel.state.set("blocked", (azazel.state.get("blocked") or 0) + 1)
			-- azazel.notify(text, level) shows a message to the user; the level is "info"
			-- (the default), "warn" or "error".
			azazel.notify("guard stopped a command of " .. ctx.agent_id .. ": " .. command, "warn")
			-- Returning { block = true, reason = ... } denies the call. The model receives the
			-- reason as the tool's error result, so say what to do instead.
			return {
				block = true,
				reason = "guard: this command is refused because " .. rule.why .. ". Do not retry it.",
			}
		end
	end
	-- A handler may instead return replacement fields. For `tool_call` these are `input` (new
	-- arguments) and `skip_prompt` (true runs the call without asking the user).
	return nil
end)

-- azazel.command{ name, description, run } adds the slash command /guard. `run` receives the
-- text typed after the command; the string it returns is shown to the user.
azazel.command({
	name = "guard",
	description = "Show how many shell commands the guard has refused",
	run = function()
		local count = azazel.state.get("blocked") or 0
		return "guard has refused " .. count .. " command(s); it watches " .. #BLOCKED .. " patterns"
	end,
})
