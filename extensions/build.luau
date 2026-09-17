--!strict
-- A build runner: the 中断・進捗付きビルド extension of the representative set
-- (docs/DEVELOPMENT-STRATEGY.md §2, 代表Luau拡張集合 ③).
--
-- It exists because a build is the production step where the agent's contract
-- is easiest to get wrong: a long command that must show progress while it
-- runs, must be interruptible without leaving the caller unsure what happened,
-- and must be resumable by checking what is already on disk instead of
-- re-running everything.
--
-- What it guarantees to its caller:
--
--   * one `on_update` per step, so a subscriber sees progress during the build
--     rather than after it (the loop drives delivery concurrently with the
--     tool);
--   * a structured result — every step's status (`ok` / `failed` / `skipped` /
--     `interrupted`), exit code and captured output — so the caller *matches*
--     the run against the plan instead of trusting a success-shaped summary;
--   * a failing step stops the build and is never reported as success;
--   * an aborted tool call stops at the next step boundary and the host kills
--     the running process (`pillar.exec` carries the call's abort signal);
--   * `resume = true` skips the steps whose artifact already exists, which is
--     what makes a build that was interrupted worth restarting.
--
-- The combined log is written through `pillar.fs.write` even when the build is
-- interrupted: observability is not something to lose at the interesting
-- moment.

local pillar = require("@pillar")

type StepResult = {
	name: string,
	status: string,
	code: number?,
	stdout: string?,
	stderr: string?,
}

local function stepStatus(result: any): string
	if result.killed then
		return "interrupted"
	end
	if result.code == 0 then
		return "ok"
	end
	return "failed"
end

local function logLine(step: any, result: any): string
	local parts = { "== " .. tostring(step.name) .. " (" .. tostring(step.command) .. ")" }
	if result.stdout ~= nil and result.stdout ~= "" then
		table.insert(parts, result.stdout)
	end
	if result.stderr ~= nil and result.stderr ~= "" then
		table.insert(parts, "stderr: " .. result.stderr)
	end
	table.insert(parts, "-> " .. tostring(result.code))
	return table.concat(parts, "\n")
end

local function summarize(results: { StepResult }): string
	local lines: { string } = {}
	for _, result in ipairs(results) do
		local line = "- " .. result.name .. ": " .. result.status
		if result.code ~= nil then
			line = line .. " (exit " .. tostring(result.code) .. ")"
		end
		table.insert(lines, line)
	end
	return table.concat(lines, "\n")
end

pillar.register_tool({
	name = "build",
	description = "Run a build plan with progress, interruption and resume",
	parameters = pillar.schema.object(
		{
			steps = pillar.schema.array(pillar.schema.object({
				name = pillar.schema.string("step name"),
				command = pillar.schema.string("program to run"),
				args = pillar.schema.array(pillar.schema.string()),
				artifact = pillar.schema.string("path that marks this step as done"),
			}, { required = { "name", "command" } })),
			resume = pillar.schema.boolean("skip the steps whose artifact exists"),
			cwd = pillar.schema.string("working directory for every step"),
			log = pillar.schema.string("file the combined log is written to"),
		},
		{ required = { "steps" } }
	),
	execute = function(tool_call_id, params, signal, on_update, ctx)
		local plan = params.steps
		local results: { StepResult } = {}
		local log: { string } = {}
		local completed = 0
		local skipped = 0
		local aborted = false

		for index, step in ipairs(plan) do
			if signal.aborted() then
				aborted = true
				break
			end

			-- Resume checks the world (the artifact), not a remembered flag:
			-- the previous run may have been interrupted, or may not have been
			-- this process at all.
			local done = params.resume == true
				and step.artifact ~= nil
				and pillar.fs.exists(step.artifact)

			if done then
				skipped = skipped + 1
				table.insert(results, { name = step.name, status = "skipped" })
			else
				local result = pillar.exec(step.command, step.args or {}, { cwd = params.cwd })
				local status = stepStatus(result)
				table.insert(log, logLine(step, result))
				table.insert(results, {
					name = step.name,
					status = status,
					code = result.code,
					stdout = result.stdout,
					stderr = result.stderr,
				})
				if status == "ok" then
					completed = completed + 1
				elseif status == "interrupted" then
					aborted = true
					break
				else
					-- A failing step ends the build; the caller gets the status
					-- and the output to explain it.
					break
				end
			end

			if on_update ~= nil then
				on_update({
					content = {
						{
							type = "text",
							text = "step " .. tostring(index) .. "/" .. tostring(#plan) .. ": " .. step.name,
						},
					},
					details = {
						step = index,
						total = #plan,
						completed = completed,
						skipped = skipped,
						aborted = aborted,
					},
				})
			end
		end

		if params.log ~= nil then
			pillar.fs.write(params.log, table.concat(log, "\n"))
		end

		return {
			content = {
				{ type = "text", text = summarize(results) },
			},
			details = {
				results = results,
				completed = completed,
				skipped = skipped,
				aborted = aborted,
			},
		}
	end,
})

return nil
