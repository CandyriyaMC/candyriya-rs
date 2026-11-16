---
name: Feature request
about: Suggest an idea or improvement
title: ""
labels: ["enhancement"]
body:
  - type: textarea
    id: problem
    attributes:
      label: Problem or motivation
      description: What problem does this solve? Who is it for?
      placeholder: As a server operator, I want ...
    validations:
      required: true

  - type: textarea
    id: proposal
    attributes:
      label: Proposed solution
      description: What would you like to see happen?
    validations:
      required: true

  - type: textarea
    id: alternatives
    attributes:
      label: Alternatives considered
      description: Any alternative solutions or workarounds you have considered.
    validations:
      required: false

  - type: textarea
    id: context
    attributes:
      label: Additional context
      description: Links, prior art (e.g. how other proxies handle this), or anything else helpful.
    validations:
      required: false
