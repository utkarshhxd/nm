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
import { resolve } from "node:path";

export const PORT = 8090;
export const BASE_URL = `http://localhost:${PORT}`;
export const ADMIN_EMAIL = "admin@example.com";
/** Only ever exists inside the per-run database that teardown drops. */
export const ADMIN_PASSWORD = "e2e-admin-password";

const NM = resolve(import.meta.dirname, "../../..", "target/release/nm");

/** `--no-dotenv`: a stray `.env` in the repo must not point sqlx at a different server. */
const SQLX = ["--no-dotenv"];

function run(file: string, args: string[], env: NodeJS.ProcessEnv, input?: string): void {
  execFileSync(file, args, { env, input, stdio: ["pipe", "inherit", "inherit"] });
}

async function waitForReady(server: ChildProcess): Promise<void> {
  for (let attempt = 0; attempt < 240; attempt++) {
    if (server.exitCode !== null) {
      throw new Error(`nm serve exited with code ${server.exitCode} before becoming ready`);
    }
    try {
      if ((await fetch(`${BASE_URL}/readyz`)).ok) return;
    } catch {
      // Not listening yet.
    }
    await new Promise((done) => setTimeout(done, 250));
  }
  throw new Error(`nm serve never answered ${BASE_URL}/readyz`);
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
  database.pathname = `/nm_e2e_${randomBytes(6).toString("hex")}`;
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
  };

  run("sqlx", ["database", "create", ...SQLX], env);

  let server: ChildProcess | undefined;
  // `-f` force-drops: a browser that is still holding a connection must not strand the database.
  const teardown = async () => {
    await stop(server);
    run("sqlx", ["database", "drop", "-y", "-f", ...SQLX], env);
    console.log(`dropped ${database.pathname.slice(1)}`);
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
