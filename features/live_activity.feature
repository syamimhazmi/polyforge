Feature: Live activity while a tab is busy
  A busy tab shows what the agent is doing, for every provider.
  This file is the acceptance spec. Unit tests are the executable check.

  Scenario: Turn-status row while busy
    Given a tab that just became busy
    Then a row above the status bar shows a braille spinner, "Thinking" and the seconds elapsed
    And the turn timer is right-aligned on that row
    And the status bar shows [busy] and no spinner

  Scenario: Live thinking block for every provider
    Given a busy tab in the Thinking phase with no pending diff
    Then the transcript ends with a "⠹ Thinking…" header
    And with no thought text the header is the only live row

  Scenario: Thought text streams under the header
    Given a busy tab in the Thinking phase receiving thought text
    Then the last 3 thought lines show under the header, indented 2 spaces
    And a "…" row shows above them when older text was cut off

  Scenario: The thinking block collapses when thinking ends
    Given a thinking block that received thought text
    When answer text, a tool call, or the end of the turn arrives
    Then one transcript line "∴ Thought for 6s" is committed
    And the thought text is not written to the transcript
    And with no thought text nothing is committed

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

  Scenario: Idle paints nothing
    Given no tab is busy
    Then there is no turn-status row and the status bar shows [idle]
    And the event loop requests no animation frames
