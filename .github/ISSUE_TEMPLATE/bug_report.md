---
name: Bug report
about: Report a reproducible bug or defect
title: ""
labels: ["bug"]
body:
  - type: textarea
    id: what-happened
    attributes:
      label: What happened?
      description: A clear and concise description of the bug.
      placeholder: When the proxy receives a login start packet...
    validations:
      required: true

  - type: textarea
    id: reproduction
    attributes:
      label: Steps to reproduce
      description: Minimal steps to reproduce the behavior.
      placeholder: |
        1. Start the proxy with ...
        2. Join with a client ...
        3. Observe ...
    validations:
      required: true

  - type: textarea
    id: expected
    attributes:
      label: Expected behavior
      description: What did you expect to happen instead?
    validations:
      required: true

  - type: textarea
    id: environment
    attributes:
      label: Environment
      description: Client version, backend server software + version, operating system, proxy version/commit.
      placeholder: |
        - Candyriya version/commit:
        - Client version:
        - Backend software (e.g. Paper 1.21.1):
        - OS:
    validations:
      required: true

  - type: textarea
    id: logs
    attributes:
      label: Relevant logs
      description: Paste relevant log output. Will be auto-formatted into code, no need for backticks.
      render: shell
    validations:
      required: false
