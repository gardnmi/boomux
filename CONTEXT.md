# Domain Glossary

## Workspace Launcher

A durable detached argument-vector command associated with a workspace and invoked on every
explicit open or restore of that workspace. Dashboard selection alone does not invoke it. Each
launcher has a durable identity and name, owns its working directory, and belongs to an ordered
workspace collection. Each invocation is ephemeral, has no PTY, and does not create or appear as a
shell or shell run.

## Shell

A durable workspace slot whose process runs are retained and observable across attachments and
daemon lifecycle transitions. A shell is distinct from a workspace launcher.
