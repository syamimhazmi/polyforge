Feature: Claude session identity across re-attach and tabs
  A killed child's late frames must not land on the replacement child.
  A session already open in one tab must not be taken by another.
  This file is the acceptance spec. Unit tests are the executable check.

  Scenario: Re-attach ignores the killed child's exit
    Given a live Claude tab whose child is generation N
    When the tab re-attaches on the same session id as generation N+1
    And the killed child later sends claude/exit for generation N
    Then the exit is dropped
    And the live tab does not print "session ended"
    And the live tab stays busy and keeps its approval

  Scenario: A second tab cannot take a session that is already open
    Given tab s1 holds a Claude store id and its remote id
    When tab s2 chooses that same stored session
    Then the chooser flashes "session is open in s1 — /tab close it first"
    And no child is queued for kill
    And no respawn is queued
    And s1 keeps its remote id
    Given that remote id is already open on another tab
    When a Claude resume of that id is attempted
    Then it fails with "session is open in another tab — /tab close it first"
    And the live child is not killed
