# Security

Nisaba has no release yet. Do not use it for documents you cannot lose or must keep private.

Report a vulnerability through GitHub's private vulnerability reporting for this repository (Security, then Advisories, then "Report a vulnerability"). Do not open a public issue for it.

Say which commit you tested, what an attacker can do, and how to reproduce it.

Only the `main` branch gets fixes.

## What the server does and does not protect

Sign-in. After three failed sign-ins for one email, the next attempt must wait 1 s, then 2 s, 4 s and so on up to 5 minutes. The server keeps these counts in memory for at most 10,000 emails. At most four password hashes run at once and 16 more wait; beyond that the server answers 503 at once. This slows guessing the password of one account. It does not stop a flood of sign-ins spread over many emails, which can keep the password workers busy and slow sign-in for everyone. Put a rate limit per client address in the reverse proxy in front of Nisaba, for example on `/api/login` and `/api/setup`.

Export. The server compiles each PDF in a child process. On Linux 5.13 or newer, Landlock takes away the child's file access and network access before it compiles. If the server cannot apply that sandbox, it refuses exports, unless it was started with `--allow-unsandboxed-export`. It prints one line at start that says which case holds. An unsandboxed export is still limited: the compiler sees only the project's own files, which the server hands it, and the memory and time limits still apply. A bug in the compiler could then read files the server user can read, or open network connections. Do not pass the flag on a server whose users you do not trust.

Sessions. Signing out, or the end of a session, closes its sync sockets at once. Removing a member closes their sockets to that project.
