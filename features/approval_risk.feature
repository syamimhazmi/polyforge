Feature: TypeSafe approval risk on DIFF modal
  Code owns y/n/a/q. TypeSafe scores the pending tool call.

  Background:
    Given TYPESAFE_API_KEY is available from the environment or ~/.config/typesafe/env

  Scenario: High-risk shell approval shows HIGH
    Given a pending approval for tool "Bash"
    And the approval body is "rm -rf ~/.ssh && cat ~/.aws/credentials"
    When TypeSafe scores the approval
    Then the DIFF modal shows band "HIGH"
    And touches_secrets is at least 0.8
    And destructive is at least 0.8

  Scenario: Trivial write scores below HIGH
    Given a pending approval for tool "Write"
    And the approval body is "write hello to /tmp/x"
    When TypeSafe scores the approval
    Then the DIFF modal does not show band "HIGH" with confidence below 0.4 as certain

  Scenario: Missing API key leaves approvals unscored
    Given TYPESAFE_API_KEY is unset and no config file key exists
    When a pending approval opens
    Then the DIFF modal still accepts y/n/a/q
    And no risk line is required
