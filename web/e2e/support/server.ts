// SPDX-License-Identifier: AGPL-3.0-only

/**
 * Playwright global setup: one throwaway NM instance for the whole run.
 *
 * A fresh `nm_e2e_<random>` database is created on the `DATABASE_URL` server, migrated, given a
 * single instance admin, and served by `nm serve` on 127.0.0.1:8090. The returned function is
 * Playwright's global teardown: it stops the server and drops that one database. No other
 * database on that server is read, written or dropped.
 *
 * `cargo build --release` must have run first; this file never builds the binary.
 */

import { type ChildProcess, execFileSync, spawn } from "node:child_process";
import { randomBytes } from "node:crypto";
import { readFileSync, rmSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";

export const PORT = 8090;
export const BASE_URL = `http://localhost:${PORT}`;
export const ADMIN_EMAIL = "admin@example.com";
/** Only ever exists inside the per-run database that teardown drops. */
export const ADMIN_PASSWORD = "e2e-admin-password";

const NM = resolve(import.meta.dirname, "../../..", "target/release/nm");

/**
 * Every database this harness has created and not yet dropped, one name per line.
 *
 * Teardown drops the database and clears this, but a SIGKILL or a cancelled job kills the
 * process before teardown runs and leaves the database behind. The next run reads this file and
 * force-drops whatever is listed, so a leak heals itself.
 *
 * shortcut: a ledger, not a scan of `pg_database` — neither `psql` nor any Postgres client is on
 * PATH here, and sqlx-cli can drop a named database but cannot list them. If the repo working
 * tree is wiped between the kill and the next run, the leak survives; swap this for a real
 * `datname like 'nm_e2e_%'` sweep the day a client library is in the image.
 */
const LEDGER = resolve(import.meta.dirname, "leaked-databases.txt");

/** `--no-dotenv`: a stray `.env` in the repo must not point sqlx at a different server. */
const SQLX = ["--no-dotenv"];

function run(file: string, args: string[], env: NodeJS.ProcessEnv, input?: string): void {
  execFileSync(file, args, { env, input, stdio: ["pipe", "inherit", "inherit"] });
}

/**
 * Exactly the shape `globalSetup` generates: `nm_e2e_` and twelve hex characters.
 *
 * The sweep below force-drops databases, so it decides for itself what it is allowed to touch
 * rather than trusting its input. The ledger is gitignored and only this file writes it today,
 * but a corrupted or hand-edited line would otherwise be a `DROP DATABASE` on `nm`, `postgres`
 * or another project's data — and this repo's standing rule is that other projects' databases
 * are never touched.
 */
export function isOurDatabase(name: string): boolean {
  return /^nm_e2e_[0-9a-f]{12}$/.test(name);
}

function ledger(): string[] {
  try {
    return readFileSync(LEDGER, "utf8").split("\n").filter(Boolean);
  } catch {
    return [];
  }
}

function saveLedger(names: string[]): void {
  if (names.length === 0) rmSync(LEDGER, { force: true });
  else writeFileSync(LEDGER, names.map((name) => `${name}\n`).join(""));
}

/** Force-drops anything a previous run left behind, and only ever a name of our own shape. */
function dropLeaked(server: URL, env: NodeJS.ProcessEnv): void {
  const kept: string[] = [];
  for (const name of ledger()) {
    if (!isOurDatabase(name)) {
      console.log(`skipping ${JSON.stringify(name)}: not a database this harness created`);
      continue;
    }
    const stale = new URL(server);
    stale.pathname = `/${name}`;
    try {
      run("sqlx", ["database", "drop", "-y", "-f", ...SQLX], { ...env, DATABASE_URL: stale.href });
      console.log(`dropped leaked ${name}`);
    } catch {
      // Unreachable server, or a drop that lost a race. Keep the entry: forgetting it here is
      // what would make the leak permanent, since nothing else records the name.
      kept.push(name);
    }
  }
  saveLedger(kept);
}

/**
 * Refuses to run if anything is already answering on the port. Without this an unrelated
 * healthy service — or an orphaned `nm` from a killed run, still pointing at a dropped
 * database — is adopted silently and every test is run against the wrong server.
 */
async function requirePortFree(): Promise<void> {
  try {
    await fetch(`${BASE_URL}/readyz`, { signal: AbortSignal.timeout(2000) });
  } catch {
    return;
  }
  throw new Error(`something is already listening on ${BASE_URL}; stop it before running the e2e suite`);
}

/** True only for this project's readiness response, so a foreign server cannot be mistaken for ours. */
async function isOurs(): Promise<boolean> {
  const res = await fetch(`${BASE_URL}/readyz`);
  if (!res.ok) return false;
  const body: unknown = await res.json();
  return (body as { database?: unknown }).database === "ok";
}

async function waitForReady(server: ChildProcess): Promise<void> {
  for (let attempt = 0; attempt < 240; attempt++) {
    if (server.exitCode !== null) {
      throw new Error(`nm serve exited with code ${server.exitCode} before becoming ready`);
    }
    try {
      if (await isOurs()) return;
    } catch {
      // Not listening yet.
    }
    await new Promise((done) => setTimeout(done, 250));
  }
  throw new Error(`nm serve never answered ${BASE_URL}/readyz the way this project's server does`);
}

async function stop(server: ChildProcess | undefined): Promise<void> {
  if (!server || server.exitCode !== null) return;
  const exited = new Promise<void>((done) => server.once("exit", () => done()));
  server.kill("SIGTERM");
  const killed = setTimeout(() => server.kill("SIGKILL"), 5000);
  await exited;
  clearTimeout(killed);
}

export default async function globalSetup(): Promise<() => Promise<void>> {
  const server_url = process.env.DATABASE_URL;
  if (!server_url) {
    throw new Error("DATABASE_URL must name the Postgres server to create the test database on");
  }

  const database = new URL(server_url);
  const name = `nm_e2e_${randomBytes(6).toString("hex")}`;
  database.pathname = `/${name}`;
  const url = database.toString();

  const env: NodeJS.ProcessEnv = {
    ...process.env,
    DATABASE_URL: url,
    NM_DATABASE_URL: url,
    NM_LISTEN: `127.0.0.1:${PORT}`,
    NM_PUBLIC_URL: BASE_URL,
    NM_SECRET_KEY: randomBytes(32).toString("base64"),
    // The migration is an explicit step below, so a failure there is not hidden inside boot.
    NM_AUTO_MIGRATE: "false",
    NM_LOG: "warn",
    // THE WHOLE SUITE SHARES ONE PER-IP RATE-LIMIT BUDGET WITHOUT THIS.
    //
    // `playwright.config.ts` runs `workers: 1` against this one server, so every request in the
    // run arrives from 127.0.0.1. The public pages (`/c/`, `/u/`, `/f/`) are limited per IP at
    // `PUBLIC_PER_MINUTE = 60`, refused requests counted — so one window of 60 would be shared by
    // every spec in the suite, and once tripped it stays tripped for the minute. `audience-admin`
    // has already lost a file to the same shape on a different limiter, and that one's escape
    // hatch ("use a fresh workspace") does not exist here, because this limiter keys on the
    // address and not on the tenant.
    //
    // With this on, `request_meta::client_ip` takes the rightmost `X-Forwarded-For` entry, so a
    // spec that sets the header gets a budget of its own:
    //
    //     test.use({ extraHTTPHeaders: { "x-forwarded-for": "198.51.100.<n>" } });
    //
    // Pick an address no other spec uses, from the documentation range (RFC 5737). A spec that
    // sets nothing keeps the peer address, which is what every spec did before this line.
    //
    // It is safe here and nowhere else: the flag means "a proxy I run sets this header", and the
    // only client of this server is the test that spawned it. A deployment must not set it unless
    // the proxy really is the operator's.
    NM_TRUST_PROXY_HEADERS: "true",
  };

  await requirePortFree();
  dropLeaked(new URL(server_url), env);

  // Recorded BEFORE it exists: a kill in the window between the two would otherwise leak a
  // database that nothing knows the name of. Sweeping a name that was never created is harmless
  // either way: the drop is a no-op, or it fails and the entry is simply kept.
  saveLedger([...ledger(), name]);
  run("sqlx", ["database", "create", ...SQLX], env);

  let server: ChildProcess | undefined;
  // `-f` force-drops: a browser that is still holding a connection must not strand the database.
  const teardown = async () => {
    await stop(server);
    run("sqlx", ["database", "drop", "-y", "-f", ...SQLX], env);
    saveLedger(ledger().filter((entry) => entry !== name));
    console.log(`dropped ${name}`);
  };

  try {
    run(NM, ["migrate"], env);
    run(
      NM,
      ["admin", "create-user", "--email", ADMIN_EMAIL, "--instance-admin"],
      env,
      `${ADMIN_PASSWORD}\n`,
    );
    server = spawn(NM, ["serve"], { env, stdio: ["ignore", "inherit", "inherit"] });
    await waitForReady(server);
  } catch (failure) {
    await teardown();
    throw failure;
  }

  return teardown;
}
