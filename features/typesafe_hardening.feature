Feature: TypeSafe client hardening
  Redirects never forward Authorization. Secrets never leave redacted.

  Scenario: Redirect responses are not followed and Authorization is not forwarded
    Given an HTTP client built for TypeSafe scoring with a Bearer token
    And a loopback server that responds 302 to a distinct landing host:port
    When the client requests the redirecting endpoint
    Then the client surfaces HTTP 302 under redirect Policy::none
    And the first hop carries Authorization Bearer for the scoring request
    And the distinct landing host:port receives zero connections
    And Authorization is never forwarded on a follow-up hop

  Scenario: Secrets in tool and body are redacted before the TypeSafe POST
    Given a TypeSafe client pointed at a loopback capture server
    And an approval tool name containing "AKIAIOSFODNN7EXAMPLE"
    And an approval body containing "sk_live_51AbCdEfGhIjKlMnOp" and "password=hunter2-hunter2"
    When judge_approval posts the approval for scoring
    Then the captured POST JSON state.tool has the secret redacted
    And the captured POST JSON state.body has the secrets redacted
    And the captured POST JSON questions remain intact for scoring
    And the raw secrets are absent from the wire body
