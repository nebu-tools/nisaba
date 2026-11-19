# Security

Nisaba has no release yet. Do not use it for documents you cannot lose or must keep private.

Report a vulnerability through GitHub's private vulnerability reporting for this repository (Security, then Advisories, then "Report a vulnerability"). Do not open a public issue for it.

Say which commit you tested, what an attacker can do, and how to reproduce it.

Only the `main` branch gets fixes.

## What the server does and does not protect

Sign-in. After three failed sign-ins for one email, the next attempt must wait 1 s, then 2 s, 4 s and so on up to 5 minutes. The server keeps these counts in memory for at most 10,000 emails. At most four password hashes run at once and 16 more wait; beyond that the server answers 503 at once. This slows guessing the password of one account. It does not stop a flood of sign-ins spread over many emails, which can keep the password workers busy and slow sign-in for everyone. Put a rate limit per client address in the reverse proxy in front of Nisaba, for example on `/api/login` and `/api/setup`.

Export. The server compiles each PDF in a child process. On Linux 6.2 or newer, with Landlock and seccomp available, the child confines itself before it compiles, and these promises hold:

- Files: it reads only what the server wrote to its standard input and writes only to its standard output. It can open, create, truncate, rename or delete no file. Landlock (ABI 3) keeps this promise.
- Sockets: it can create no socket of any kind, network or Unix, and holds no file descriptor but its standard input, output and error. A seccomp filter keeps this promise; Landlock is not what stops the network.
- Processes: it cannot signal, trace or read the memory of another process, or change another process's limits or scheduling. The seccomp filter keeps this promise.
- Privileges: it runs with `no_new_privs`.

If the server cannot keep every promise, it refuses exports, unless it was started with `--allow-unsandboxed-export`. It prints one line at start that says which case holds. An unsandboxed export keeps the rest of its limits: the compiler sees only the project's own files, which the server hands it, and the memory, CPU and time limits still apply. It loses the promises: a bug in the compiler could then read or change files the server user can, open network connections, or signal the server. Do not pass the flag on a server whose users you do not trust.

Sessions. Signing out, signing out everywhere, a password change in another session, an expired session, or a deleted account closes the session's sync sockets at once (close code 4401). Removing a member, or deleting the project, closes their sockets to that project (4004); a role change that takes away writing closes them with 4003. A session ends 30 days after its last renewal and at most 90 days after sign-in. A password change needs the current password, counts a wrong one like a failed sign-in, and ends every other session of the account.

Passwords. They are hashed with Argon2id, with at least the `argon2` crate's default parameters (19 MiB, 2 passes). Raising `passwords.memory_kib` or `passwords.iterations` makes each sign-in replace an older hash with a stronger one.

Requests. Every request other than `GET` and `HEAD` must carry an `Origin` header equal to the public address, and so must the sync socket's handshake; there are no other CSRF tokens. The session cookie is `HttpOnly` and `SameSite=Lax`, and `Secure` with the `__Host-` prefix behind https. Every answer carries a Content Security Policy that lets the page load and connect to this server only and never be framed, with `nosniff`, `X-Frame-Options: DENY` and `Referrer-Policy: no-referrer`; behind https it also carries `Strict-Transport-Security`. An uploaded asset is served as `application/octet-stream` under `Content-Security-Policy: sandbox`, so an HTML or SVG file cannot run as a page of the server.

What others can learn. An owner who adds a member by email learns whether an account has that email. The administrator sees every account's email and name, and no project: being an administrator gives no access to projects.

Logs. The server logs requests by method and path, without the query, and people by their account id. It logs no password, session token, email address or document text.

Packages. When a document imports a package the server's cache lacks, the server downloads it from the package registry (`packages.registry`, https://packages.typst.org by default), so the registry learns which packages the server's documents use and when. `--no-package-downloads` turns this off. A downloaded package is checked for its size and for paths that leave it before it is stored. Only a signed-in person can make the server fetch a package.

Data. One server process uses a data directory; a second refuses to start. `nisaba backup` copies the database and the files and checks each asset against its hash; the backup holds every account's password hash and every document, so keep it as private as the data directory.
