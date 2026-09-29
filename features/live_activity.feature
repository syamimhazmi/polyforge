Feature: Live activity while a tab is busy
  A busy tab shows what the agent is doing, for every provider.
  This file is the acceptance spec. Unit tests are the executable check.

  Scenario: Turn-status row while busy
    Given a tab that just became busy
    Then a row above the input box, starting at column 4, shows a braille spinner, "Thinking…" and the phase timer with one decimal (1.4s)
    And the turn timer, the context tokens and [stop] are right-aligned on that row
    And the shortcuts row shows [busy] and no spinner

  Scenario: Live thinking block for every provider
    Given a busy tab in the Thinking phase with no pending diff
    Then the transcript ends with a "◆ Thinking…" header
    And with no thought text the header is the only live row

  Scenario: Thought text streams under the header
    Given a busy tab in the Thinking phase receiving thought text
    Then the last 3 thought lines show under the header, indented 2 spaces
    And a "…" row shows above them when older text was cut off

  Scenario: The thinking block collapses when thinking ends
    Given a thinking block that received thought text
    When answer text, a tool call, or the end of the turn arrives
    Then one transcript line "◆ Thought for 6s" is committed
    And the thought text is not written to the transcript
    And with no thought text nothing is committed

  Scenario: A finished turn commits one Worked-for line
    Given a busy tab whose turn ends normally
    Then one transcript line "Worked for 5.2s" is committed and no per-provider "done ✓" line is
    And a turn ended by a stop request commits "■ stopped" and no Worked-for line

  Scenario: Codex reasoning is a thought, not an answer
    Given a codex reasoning item arrives
    Then it feeds the thinking block and not the transcript as answer text

  Scenario: First answer text switches the phase
    Given a busy tab in the Thinking phase
    When the first answer text arrives
    Then the turn-status row shows "Responding"
    And the phase timer restarts while the turn clock keeps running

  Scenario: Pinned view stays pinned
    Given the transcript is scrolled to the bottom
    When the live block grows, collapses, or the turn-status row appears
    Then the view stays at the bottom
    But a view scrolled up is not moved

  Scenario: The rail marks only the current turn
    Given a busy tab with earlier turns and the user's prompt in the transcript
    Then a heavy rail "┃" overdraws the left border on rows of the current turn's output
    And earlier turns and the "> " prompt rows keep the normal border
    And the wrapping and scroll math are unchanged

  Scenario: Approval pending stops the animation
    Given a busy tab with a pending diff
    Then the turn-status row shows a static "◆ awaiting approval"
    And no thinking block and no rail are drawn

  Scenario: Token count and stop button on the turn row
    Given a busy tab whose backend reported a context size of 12,345 tokens
    Then the right side of the turn-status row reads "1m 20s ⇣12.3k [stop]"
    And with no token report yet there is no "⇣" segment
    And on a narrow terminal the tokens, then the timers, then the label give way first
    And the two sides of the row never overlap

  Scenario: Stopping a turn with Esc
    Given a busy tab with no pending diff in Normal mode
    When I press Esc
    Then one stop is queued for that tab's backend and the row shows "Stopping…" with its own timer
    And when the backend ends the turn exactly one "■ stopped" line is added
    But Esc on an idle tab does nothing

  Scenario: Stopping a turn by clicking [stop]
    Given a busy tab with no pending diff and mouse capture on
    When I left-click the "[stop]" cells
    Then the tab behaves as if Esc was pressed
    And a click anywhere else still starts a text selection

  Scenario: A second stop press forces the stop
    Given a tab that is stopping but whose backend has not ended the turn
    When I press Esc or click "[stop]" again
    Then the tab goes idle and one "■ stopped (forced; late output may still arrive)" line is added
    And late status events do not make the tab busy again until my next prompt

  Scenario: Approvals are not stoppable and are declined while stopping
    Given a busy tab with a pending diff
    Then the row shows "◆ awaiting approval" with no "[stop]" and Esc still defers the card
    And an approval that arrives while a stop is in flight is declined with "stop: declined <tool> approval"

  Scenario: Each backend has a stop path
    Given a busy tab on muse, codex, grok, claude, agy or mock
    When I stop it
    Then muse and codex send turn/interrupt, grok sends session/cancel, claude sends an interrupt control request, agy gets SIGINT, and mock stops at once
    And a codex turn with no known turn id says "turn not started yet — try again" instead of stopping

  Scenario: Idle paints nothing
    Given no tab is busy
    Then there is no turn-status row and the shortcuts row shows [idle]
    And the event loop requests no animation frames
