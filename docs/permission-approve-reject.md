# Permission approve/reject (removed with the classic TUI)

The classic session-first TUI (`src/tui/`, deleted in the cutover to the
conversation-first model) let you approve or reject a Claude permission prompt
**from the dashboard**, without switching into the session. The conversation-first
TUI intentionally does **not** carry this over — when a conversation shows
`NeedsPermission`, you press Enter to switch into it and answer in Claude directly.

This note records how it worked, so it can be rebuilt against the conversation
model if we want it back.

## The mechanism

1. **Hotkey assignment.** Each session whose status was `NeedsPermission`,
   `PlanReview`, or `EditApproval` was assigned a letter from a fixed pool:

   ```rust
   pub const PERMISSION_KEYS: [char; 6] = ['y', 'z', 'x', 'w', 'v', 't'];
   ```

   Assignment was **stable per session** across refreshes via a
   `permission_key_map: HashMap<sessionName, char>` on the `App`: keep a session's
   existing key if it still needs permission, free keys of sessions that no longer
   do, and hand an unused key to any newly-needy session. The assigned letter was
   shown on the session's row.

2. **Approve / approve-always.** Pressing the **lowercase** letter approved
   (select option **1** = "Yes"); pressing the **uppercase** letter approved-always
   (option **2** = "Yes, and don't ask again"), but only when the status was
   `NeedsPermission` (plan/edit prompts have no always-option).

3. **Delivery.** The keystrokes were sent straight to the Claude **pane** that owned
   the prompt (resolved from `SessionInfo.claude_pane` = `(session, window, pane)`):

   ```rust
   let keys = if is_uppercase && has_approve_always {
       vec!["2", "Enter"]           // "Yes, and don't ask again"
   } else {
       vec!["1", "Enter"]           // "Yes"
   };
   for key in &keys {
       send_key_to_pane(sess, win, pane, key);   // tmux send-keys -t sess:win.pane <key>
   }
   ```

4. **Optimistic UI.** The just-approved session was added to a `pending_approvals`
   set and its selection hidden, so it immediately read as busy rather than fl: the
   status re-derived to "working" on the next refresh once Claude moved on.

The reject path was analogous — it would send the "No" option instead (option
number depends on the prompt). Only approve/approve-always were wired in the end.

## To rebuild it on the conversation model

The building blocks all still exist:

- A conversation's live window is `Conversation.placement` = `(session_name,
  window_index, window_name, pane_id)`, and its status is in `Conversation.status`
  (`SessionStatus::NeedsPermission` / `PlanReview` / `EditApproval`). That's the
  same `(session, window, pane)` target `send_key_to_pane` needs.
- Assign a hotkey per **needy conversation** (not session) with the same stable-map
  approach, keyed by conversation id; render it on the row; on keypress send
  `1`/`2` + `Enter` (or the reject option) to `placement.pane_id`.
- The natural home is the Active view key handler in `src/cli/conversations.rs`,
  alongside the existing `s`/`v`/`m`/`!` session-flag keys.

The reason it was dropped, not ported: the user reported never using it, and the
fallback (Enter into the session, answer in Claude) is one keystroke away.
