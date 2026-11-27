# 0011. Let each gateway choose e-mail and password sign-in or SSO

- Date: 2026-11-27
- Status: Accepted, not implemented

## Context

This decision adds a second way to sign in to the dashboard: an e-mail address and a password. Each
gateway chooses in its config either SSO or passwords. It never uses both.

Today a person can sign in only through an external SSO service:

- The SSO service shows the sign-in screen and issues a signed sign-in token (a JWT). The gateway
  checks the signature with the keys that the SSO service publishes (JWKS). Then it finds the
  person's account by the token's issuer and subject.
- The dashboard renews the token through the SSO refresh address, and signs out through the SSO
  sign-out address.
- The config decides who can use the gateway: `allowed_emails` and `admin_emails`.

A team without an SSO service cannot use the gateway. Such a team needs a way where an admin
invites a person by e-mail, and the person then signs in with an e-mail address and a password.

Decision [0001](0001-access-list-in-the-database.md) is accepted but not implemented. It moves the
access list from the config to the database, and it keeps one admin in the config. It also lets an
admin add a person before that person signs in for the first time. That step is the same as an
invitation, so this decision builds on 0001.

In this document, an **account** is the gateway's database record of a person (the `gw_principals`
table). Keys, policies and usage belong to an account.

## Decision

1. **One mode per gateway.** The config value `auth.mode` (environment variable `AUTH_MODE`) is
   `sso` or `password`. It has no default.
   - In `sso` mode, the SSO values are required: `AUTH_ISSUER`, `AUTH_JWKS_URI`, `SSO_AUDIENCE`,
     `SSO_ACCESS_COOKIE_NAME`, `SSO_LOGIN_URL`, `SSO_REFRESH_URL`, `SSO_LOGOUT_URL` and the optional
     `SSO_RETURN_PARAM`.
   - In `password` mode, the SMTP values of rule 6 are required.
   - A value of the other mode must not be set. If one is set, or a required value is missing, the
     gateway does not start, and the error names the value.
2. **Decision 0001 is implemented first, in both modes.** The access list is in the database and is
   keyed by e-mail address. In `sso` mode, an admin adds a person to the list. In `password` mode,
   adding a person also sends an invitation. `allowed_emails` is removed. `admin_emails` becomes the
   single `admin_email`, and admins can make other people admins on the dashboard.
3. **Identity in `password` mode.** The gateway is its own source of identity. A person's issuer is
   the gateway's own address (the `public_origin` config value), and the subject is the lower-case
   e-mail address. The gateway makes the account when the person sets a password for the first time.
   In `sso` mode, the first sign-in makes the account in the same way.
4. **Passwords.**
   - A password has 12 to 128 characters. There are no rules about character kinds.
   - The gateway stores only an argon2id hash, with 19 MiB of memory, 2 passes and no parallel
     threads.
   - Only the person can set or change their own password. An admin cannot see or set it.
5. **Links.** The gateway makes three kinds of one-use links: an invitation, a password reset, and the
   setup link of rule 8.
   - Each link holds a random 32-byte token in the URL fragment (`/dashboard/password#token=...`),
     so the token does not reach server logs or the `Referer` header. The database keeps only a
     SHA-256 hash of the token.
   - An invitation is valid for 7 days. A reset link and a setup link are valid for 24 hours.
   - When a person sets a password, all open links of that person stop working at once.
   - When an admin sends a new invitation or reset link to a person, the earlier links of that
     person stop working.
   - An admin sends a reset link from the user list. There is no "forgot password" on the sign-in
     screen.
6. **Mail.** The gateway sends invitations and reset links through an SMTP server.
   - Config values: `SMTP_HOST`, `SMTP_PORT`, `SMTP_USERNAME`, `SMTP_PASSWORD`, `SMTP_FROM`.
   - The connection must use TLS (TLS from the start on port 465, or STARTTLS). The gateway refuses
     a server that offers neither.
   - The gateway stores the link first and then sends the mail. If the server refuses the mail or
     does not answer in 10 seconds, the gateway removes the stored link. The admin's invitation or
     reset request then answers `502 mail_failed`, and the admin can try again.
7. **Sign-in and sessions.**
   - `POST /auth/password` takes an e-mail address and a password. On success, the gateway makes a
     session and sets a session cookie (`HttpOnly`, `Secure`, `SameSite=Lax`). The cookie holds a
     random token. The database keeps only its hash.
   - A session ends in these cases:
     - 12 hours after sign-in;
     - at sign-out (`POST /auth/logout`);
     - when the person changes the password;
     - when an admin suspends the account on the dashboard (decision
       [0002](0002-account-suspension.md)), or removes the person from the access list.

     The gateway checks the session on each request, so each end applies at once.
   - A wrong password and an unknown e-mail address get the same answer and take the same time. For
     an unknown address, the gateway checks the password against a fixed dummy hash.
   - After 5 failed attempts for one e-mail address, that address cannot sign in for 15 minutes.
   - The gateway already limits how many authentication checks run at the same time. Password hash
     checks count toward the same limit.
   - The CSRF check stays as it is. Its token is now tied to the SSO sign-in token. In `password` mode
     it is tied to the session token.
8. **The first admin.** The **fixed admin** is the person named by `admin_email`. In `password`
   mode, while the fixed admin has no password, the gateway writes a setup link to its log at each
   start, at warning level.
   - Each start makes a new setup link. This is an exception to rule 5: the earlier setup links stay
     valid until they expire, so that several instances can start at the same time.
   - All setup links stop working when the fixed admin sets a password (rule 5).
   - If no admin can sign in (for example, every admin has lost the password and no admin can send
     a reset link), the operator changes `admin_email` to an address that has no password yet and
     restarts. At each start, the gateway adds `admin_email` to the access list as an admin if it is
     not there. The new fixed admin then gets a setup link in the log.
9. **Dashboard.**
   - In `password` mode, `/auth/login` sends the browser to the dashboard's sign-in screen, not to
     an SSO service.
   - New screens: sign-in, and setting a password from a link.
   - The admin's user list gets "invite" and "send reset link".
   - `GET /api/me` tells the dashboard the mode. In `password` mode there is no refresh address. The
     dashboard shows the sign-in screen when a session ends.
10. **Out of scope:** two-factor sign-in, a self-service password reset, a change of e-mail address,
    and moving accounts from one mode to the other. After a mode change, the existing accounts stay
    in the database but cannot sign in.

## Rejected alternatives

**Allow SSO and passwords at the same time.** Rejected because one gateway serves one team, and two
ways to sign in double the screens and the security checks.

**The admin copies the invitation link and sends it.** Rejected because the invitation must reach
the person without a manual step.

**A "forgot password" mail on the sign-in screen.** Rejected because anyone who knows an e-mail
address could make the gateway send mail. An admin can send a reset link when a person asks.

**Get the setup link through a maintenance command.** Rejected because the operator would need a
shell on the server. The start log is available on the hosting platform.

**A signed session token without a database row (like a JWT).** Rejected because a session must end
at once on sign-out, on a password change and on suspension.

## Consequences

- While the fixed admin has no password, the start log holds a secret link. Anyone who can read the
  log can set the fixed admin's password until the password is set or the link expires. Only
  operators must be able to read the logs on the hosting platform.
- The config change is breaking: `AUTH_MODE` is new, `allowed_emails` is removed and `admin_emails`
  becomes `admin_email`. The deploy config of every gateway must change.
- Because admins can make other admins (rule 2), one stolen admin session can add more admins. Every
  change to the access list is written to the audit log.
- New dependencies: `argon2` for the hashes and `lettre` for SMTP.
- New tables: the access list of 0001, password hashes, links, sessions, and failed sign-in counts.
- `README.md` describes both modes, the SMTP values and the first-admin start. Decision 0001 is marked
  `Implemented` together with this decision.
