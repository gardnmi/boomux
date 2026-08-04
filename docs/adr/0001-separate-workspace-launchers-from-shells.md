# Separate Workspace Launchers From Shells

Workspace launchers are durable workspace configuration with ephemeral, detached invocations;
they are not auto-deleting or hidden shells. This preserves shells as observable durable process
slots while allowing every explicit workspace open to start desktop applications without PTYs,
retained runs, or lifecycle ownership.
