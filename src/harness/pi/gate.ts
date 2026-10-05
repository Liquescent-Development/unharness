// Loaded into pi by unharness (`-e`). pi runs its tools without asking;
// this puts every call that is not a read to the client first, as a select
// dialog whose title carries the call. unharness answers it by its policy,
// its allow rules or the user. Anything but "Allow" blocks the tool, and
// so does pi itself when this handler throws.
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

const PREFIX = "unharness-gate:";
const READS = new Set(["read", "grep", "find", "ls"]);

export default function (pi: ExtensionAPI) {
	pi.on("tool_call", async (event, ctx) => {
		if (READS.has(event.toolName)) return undefined;
		if (!ctx.hasUI) {
			return { block: true, reason: "Blocked: there is no one to ask (unharness policy ask)" };
		}
		const call = { toolCallId: event.toolCallId, toolName: event.toolName, input: event.input };
		// Over RPC the client reads the call; in pi's own interface a person does.
		const title =
			ctx.mode === "rpc"
				? PREFIX + JSON.stringify(call)
				: `Allow ${event.toolName}?\n\n${JSON.stringify(event.input, null, 2)}`;
		const choice = await ctx.ui.select(title, ["Allow", "Deny"]);
		if (choice !== "Allow") {
			return { block: true, reason: "Denied by the user" };
		}
		return undefined;
	});
}
