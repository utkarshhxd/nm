# Run NM on your own computer

This starts the whole product (the app, its database, a fake mail server and a local HTTPS front
door) with one command. You need **Docker Desktop** running. Nothing else.

Everything below is typed in a terminal opened in this folder (PowerShell, Git Bash or any shell).

## 1. Settings file

```sh
cp .env.example .env          # PowerShell: Copy-Item .env.example .env
```

Open `.env` and set two things:

- `POSTGRES_PASSWORD`: any letters and digits.
- `NM_SECRET_KEY`: a random key. It encrypts relay passwords and DKIM keys stored in the database.
  Make one:

  ```powershell
  # PowerShell
  $b = New-Object byte[] 32; (New-Object Security.Cryptography.RNGCryptoServiceProvider).GetBytes($b); [Convert]::ToBase64String($b)
  ```
  ```sh
  # Git Bash, Linux, macOS
  openssl rand -base64 32
  ```

  Paste the output after `NM_SECRET_KEY=`. Keep `.env` private; it is not committed to git.
  Do not change the key later: data encrypted with the old one becomes unreadable.

## 2. Start it

```sh
docker compose up -d --build
```

The first build compiles the Rust app and the web UI, and takes a good while (10 to 30 minutes).
Later starts are quick. The build uses two CPU jobs so it fits in 8 GB of memory; set
`CARGO_BUILD_JOBS=4` in `.env` if Docker has more.

Check it is up: open <https://localhost/healthz>. It should say `ok`.

Ports used on your machine: **443** (the app, through Caddy) and **8025** (Mailpit's inbox). Both
listen on `127.0.0.1` only, so other computers on your network cannot reach them. If something
else already uses 443, change `"127.0.0.1:443:443"` to `"127.0.0.1:8443:443"` in
`docker-compose.yml`, and also set `NM_PUBLIC_URL: https://localhost:8443` for the `nm` service and
use `https://localhost:8443` everywhere below.

## 3. Open the app and make the first account

Go to <https://localhost>. Your browser warns that the certificate is not trusted. That is expected:
Caddy made it locally. Choose Advanced, then continue to localhost.

There is no sign-up page. Create the first account from the command line (it asks for nothing; the
password is the first line of input):

```sh
echo 'pick a long password here' | docker compose exec -T nm nm admin create-user --email you@example.com --instance-admin
```

Sign in at <https://localhost> with that email and password. Then create a **workspace** (your
company or project) in the UI.

## 4. Add Mailpit as a relay

A relay is the SMTP server NM sends through. Mailpit catches mail instead of delivering it, so
nothing leaves your computer.

In the workspace, go to Relays and add one:

| Field | Value |
| --- | --- |
| Host | `mailpit` |
| Port | `1025` |
| Security | **STARTTLS** |
| Username / password | leave empty |
| Rate limit per minute | `600` |

**Security must be STARTTLS, not "none".** NM has no plain-text option on purpose: it will not send
mail over an unencrypted connection. The local setup handles this for you. A one-time helper
container creates a private certificate authority and a certificate for `mailpit`, Mailpit uses
it, and NM is told to trust it. You do not do anything.

## 5. Add a sending domain

Yes, a sending domain is needed: campaigns are sent "from" an address on a domain you registered
in the Sending domains page.

1. Add a domain, for example `example.test`. Signing: **relay** (so no DKIM key is needed).
2. A real domain must pass DNS checks (SPF, DKIM, return path) before NM will send from it.
   Locally there is no DNS, so the checks will not pass. NM lets an admin accept that: open the
   domain and **acknowledge sending while checks are not passing**. This is the app's own
   setting, and the check stays visible as failing.
3. Before a campaign can be sent, the workspace's **Brand kit** needs a postal address in the
   footer (a legal requirement for marketing mail). Any text works locally.

## 6. Send a test campaign

1. Create a list, and add a contact in it using an address of your choice, for example
   `reader@example.com`. The list asks for double opt-in by default; turn that off on the list, or
   confirm the contact, so the contact is mailable.
2. Create a template (blocks, MJML or HTML) and include a link.
3. Create a campaign: pick the list, the sending domain, a from name and local part, a subject
   and the template. Use **Send test** to try it, or schedule it to send now.
4. Open <http://localhost:8025>. The message appears in Mailpit's inbox (a test send is marked
   `[Test]`). The unsubscribe link in the footer goes to `https://localhost/u/...`, which is NM;
   click it (accept the certificate warning again if asked).
5. A campaign message is tracked. It carries a 1×1 image from `https://localhost/o/...`, so
   opening it with images shown records an open. Each link you put in the template becomes
   `https://localhost/l/...`: a click goes through NM, which records it and redirects to the
   address you wrote. A test send keeps its links as written and records nothing.
6. Open the campaign in NM. Its **Report** section shows what was sent, failed and bounced, the
   opens and clicks, the clicks on each link and the unsubscribes. Press **Refresh** to update it.

## Stop, restart, reset

```sh
docker compose down        # stop and remove the containers; your data stays
docker compose up -d       # start again
docker compose down -v     # stop and ERASE everything (database, certificates)
```

Logs: `docker compose logs -f nm`.

## Troubleshooting

- `required variable ... is missing`: `.env` is missing or `NM_SECRET_KEY` is empty.
- The page will not load: `docker compose ps`; all of `postgres`, `nm`, `caddy`, `mailpit` should be
  running. `docker compose logs nm` shows why `nm` is not.
- Messages queued but not arriving: check Mailpit's host/port/security above and
  `docker compose logs nm`.
