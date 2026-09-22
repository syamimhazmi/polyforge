Feature: Clipboard / OSC leak surface hardening
  OSC 52 never carries huge selections. The backup file stays in-tree and
  never follows symlinks.

  Scenario: Oversize selections skip the OSC 52 leg
    Given a selection under 100 KiB of raw text
    When the OSC 52 gate is checked
    Then the leg fires normally
    Given a selection over 100 KiB of raw text
    When the OSC 52 gate is checked
    Then the OSC 52 leg is skipped
    And the native/tmux legs and the backup file still receive the text

  Scenario: Backup path is restricted and symlinks refused
    Given no POLYFORGE_COPY_FILE override
    When the backup path resolves
    Then it is $XDG_DATA_HOME/polyforge/last-copy.txt
    Given POLYFORGE_COPY_FILE=/tmp/evil.txt or a `..` traversal outside the subtree
    When the backup path resolves
    Then it is rejected with InvalidInput
    Given a relative POLYFORGE_COPY_FILE
    When the backup path resolves
    Then it is remapped under $XDG_DATA_HOME/polyforge/
    Given a symlink at the backup path
    When the backup write runs
    Then it fails with PermissionDenied and the link target is untouched
    Given a symlink at the `polyforge` directory or at a subdirectory under it
    When the backup write runs
    Then it fails with PermissionDenied and the outside directory is unchanged

  Scenario: Backup retention is single-file overwrite
    Given two consecutive copies
    When each backup write completes
    Then the file holds only the second copy
    And no rotation siblings exist beside last-copy.txt
    And the file mode is 0600
