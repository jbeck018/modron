// The portal's Directory: one SQLite-backed Durable Object (idFromName
// "directory") holding the owner's session list, owner sign-in cookies,
// invite records, one-time connection tickets and sign-in rate counters.
// Only RPC methods on this class reach it; no HTTP route is wired to it.
// Each method has no await between its reads and writes, so it is atomic
// with respect to other RPCs on this instance (the ticket redeem relies on
// that: read, delete, then decide).
//
// Time: every method that depends on the clock takes `now` (epoch ms) from
// the router, which reads it once per request (requestNow in src/index.ts;
// the x-test-now override exists only under MODRON_TEST_HOOKS).
//
// Stale sessions: a registration older than SESSION_GC_MS (30 days) is
// deleted by a lazy sweep at the start of every session read or write
// (listSessions, upsertSession, getSession, deleteSession, issueTicket). No
// alarm is armed, so nothing needs re-arming. A session that is stale for 5
// minutes is still stored; listSessions reports it offline (portal.ts
// sessionView).

import { DurableObject } from "cloudflare:workers";
import {
  AUTH_MAX_ATTEMPTS,
  AUTH_WINDOW_MS,
  OWNER_TTL_MS,
  SESSION_GC_MS,
  TICKET_TTL_MS,
  randomHex,
} from "./portal";

export type Role = "viewer" | "approver";

export type SessionRow = {
  id: string;
  title: string;
  host_label: string;
  host_kind: string;
  viewer_key: string;
  state: string;
  updated_at: number;
};

export type InviteRow = {
  id: string;
  session_id: string;
  role: Role;
  expires_at: number;
  revoked: number;
  created_at: number;
  uses: number;
};

export type PushRow = {
  id: string;
  endpoint: string;
  origin: string;
  p256dh: string;
  auth: string;
  role: "owner" | "approver";
  session_id: string | null;
  created_at: number;
};

export type IssuedTicket = { ticket: string; viewer_key: string; role: Role };

// A linked GitHub account (PLAN 10.10). The token is sealed (vault.ts,
// associated data = the account id); the row never holds the plaintext.
export type GithubAccountRow = {
  id: string;
  login: string;
  kind: string;
  nonce: ArrayBuffer;
  ciphertext: ArrayBuffer;
  created_at: number;
};

export type RunView = RunRow & { viewer_key: string | null };

// A cloud run (runs.ts). The git token is not stored: it is unsealed from the
// account at each wake.
export type RunRow = {
  id: string;
  repo: string;
  account_id: string;
  branch: string;
  state: string;
  reason: string | null;
  created_at: number;
  session_id: string | null;
  // The handoff ref the run was started with ("" = none), so a wake after a
  // stop imports the same handoff again.
  handoff_ref: string;
};

// A stored provider key (vault.ts): ciphertext only, never the plaintext.
export type CredentialRow = {
  provider_id: string;
  nonce: ArrayBuffer;
  ciphertext: ArrayBuffer;
  updated_at: number;
};

export class Directory extends DurableObject<object> {
  private readonly sql: SqlStorage;

  constructor(state: DurableObjectState, env: object) {
    super(state, env);
    this.sql = state.storage.sql;
    void state.blockConcurrencyWhile(async () => {
      this.sql.exec(`CREATE TABLE IF NOT EXISTS sessions (
        id TEXT PRIMARY KEY,
        title TEXT NOT NULL,
        host_label TEXT NOT NULL,
        host_kind TEXT NOT NULL,
        viewer_key TEXT NOT NULL,
        state TEXT NOT NULL,
        updated_at INTEGER NOT NULL)`);
      // cookie_id is SHA-256 of the cookie value: a leaked row is not a session.
      this.sql.exec(`CREATE TABLE IF NOT EXISTS owner_sessions (
        cookie_id TEXT PRIMARY KEY,
        created_at INTEGER NOT NULL,
        last_seen INTEGER NOT NULL)`);
      this.sql.exec(`CREATE TABLE IF NOT EXISTS invites (
        id TEXT PRIMARY KEY,
        session_id TEXT NOT NULL,
        role TEXT NOT NULL,
        expires_at INTEGER NOT NULL,
        revoked INTEGER NOT NULL DEFAULT 0,
        created_at INTEGER NOT NULL,
        uses INTEGER NOT NULL DEFAULT 0)`);
      this.sql.exec("CREATE INDEX IF NOT EXISTS invites_by_session ON invites(session_id)");
      this.sql.exec(`CREATE TABLE IF NOT EXISTS tickets (
        ticket TEXT PRIMARY KEY,
        viewer_key TEXT NOT NULL,
        role TEXT NOT NULL,
        expires_at INTEGER NOT NULL)`);
      this.sql.exec(`CREATE TABLE IF NOT EXISTS auth_attempts (
        ip TEXT PRIMARY KEY,
        window_start INTEGER NOT NULL,
        attempts INTEGER NOT NULL)`);
      // Web Push subscriptions (src/push.ts). id = SHA-256 of the endpoint.
      // session_id NULL = the owner's device (every session); otherwise an
      // approver's invite covers that one session.
      this.sql.exec(`CREATE TABLE IF NOT EXISTS push_subscriptions (
        id TEXT PRIMARY KEY,
        endpoint TEXT NOT NULL,
        origin TEXT NOT NULL,
        p256dh TEXT NOT NULL,
        auth TEXT NOT NULL,
        role TEXT NOT NULL,
        session_id TEXT,
        created_at INTEGER NOT NULL)`);
      // Provider keys (vault.ts). Sealed with AES-GCM under VAULT_KEY; the
      // Directory never sees the key material, only nonce and ciphertext.
      this.sql.exec(`CREATE TABLE IF NOT EXISTS credentials (
        provider_id TEXT PRIMARY KEY,
        nonce BLOB NOT NULL,
        ciphertext BLOB NOT NULL,
        updated_at INTEGER NOT NULL)`);
      // GitHub accounts (github.ts): token sealed, login and kind in clear.
      this.sql.exec(`CREATE TABLE IF NOT EXISTS github_accounts (
        id TEXT PRIMARY KEY,
        login TEXT NOT NULL,
        kind TEXT NOT NULL,
        nonce BLOB NOT NULL,
        ciphertext BLOB NOT NULL,
        created_at INTEGER NOT NULL)`);
      // Cloud runs (runs.ts). session_id is set when the host registers a
      // session carrying this run id (see linkRunSession).
      this.sql.exec(`CREATE TABLE IF NOT EXISTS runs (
        id TEXT PRIMARY KEY,
        repo TEXT NOT NULL,
        account_id TEXT NOT NULL,
        branch TEXT NOT NULL,
        state TEXT NOT NULL,
        reason TEXT,
        created_at INTEGER NOT NULL,
        session_id TEXT,
        handoff_ref TEXT NOT NULL DEFAULT '')`);
    });
  }

  // --- owner sign-in ---

  // Counts one sign-in attempt for `ip`. Returns false once the window
  // already holds AUTH_MAX_ATTEMPTS attempts.
  async takeAuthAttempt(ip: string, now: number): Promise<boolean> {
    this.sql.exec("DELETE FROM auth_attempts WHERE window_start <= ?", now - AUTH_WINDOW_MS);
    const row = this.sql
      .exec<{ window_start: number; attempts: number }>("SELECT window_start, attempts FROM auth_attempts WHERE ip = ?", ip)
      .toArray()[0];
    if (!row) {
      this.sql.exec("INSERT INTO auth_attempts (ip, window_start, attempts) VALUES (?, ?, 1)", ip, now);
      return true;
    }
    if (row.attempts >= AUTH_MAX_ATTEMPTS) return false;
    this.sql.exec("UPDATE auth_attempts SET attempts = attempts + 1 WHERE ip = ?", ip);
    return true;
  }

  async createOwnerSession(cookieHash: string, now: number): Promise<void> {
    this.sql.exec("INSERT INTO owner_sessions (cookie_id, created_at, last_seen) VALUES (?, ?, ?)", cookieHash, now, now);
  }

  // True when the cookie is live (under 30 days old). Expired rows are removed.
  async touchOwnerSession(cookieHash: string, now: number): Promise<boolean> {
    const row = this.sql
      .exec<{ created_at: number }>("SELECT created_at FROM owner_sessions WHERE cookie_id = ?", cookieHash)
      .toArray()[0];
    if (!row) return false;
    if (now - row.created_at >= OWNER_TTL_MS) {
      this.sql.exec("DELETE FROM owner_sessions WHERE cookie_id = ?", cookieHash);
      return false;
    }
    this.sql.exec("UPDATE owner_sessions SET last_seen = ? WHERE cookie_id = ?", now, cookieHash);
    return true;
  }

  async revokeOwnerSession(cookieHash: string): Promise<void> {
    this.sql.exec("DELETE FROM owner_sessions WHERE cookie_id = ?", cookieHash);
  }

  // --- session directory ---

  // Deletes registrations not refreshed for SESSION_GC_MS. Runs first in
  // every session-touching method, so a sweep happens on the next access.
  private sweepSessions(now: number): void {
    this.sql.exec("DELETE FROM sessions WHERE updated_at <= ?", now - SESSION_GC_MS);
  }

  async upsertSession(row: SessionRow, now: number): Promise<void> {
    this.sweepSessions(now);
    this.sql.exec(
      `INSERT OR REPLACE INTO sessions (id, title, host_label, host_kind, viewer_key, state, updated_at)
       VALUES (?, ?, ?, ?, ?, ?, ?)`,
      row.id, row.title, row.host_label, row.host_kind, row.viewer_key, row.state, row.updated_at,
    );
  }

  async deleteSession(id: string, now: number): Promise<boolean> {
    this.sweepSessions(now);
    const existed = this.sql.exec("SELECT 1 AS one FROM sessions WHERE id = ?", id).toArray().length > 0;
    this.sql.exec("DELETE FROM sessions WHERE id = ?", id);
    return existed;
  }

  async getSession(id: string, now: number): Promise<SessionRow | null> {
    this.sweepSessions(now);
    return this.sql.exec<SessionRow>("SELECT * FROM sessions WHERE id = ?", id).toArray()[0] ?? null;
  }

  async listSessions(now: number): Promise<SessionRow[]> {
    this.sweepSessions(now);
    return this.sql.exec<SessionRow>("SELECT * FROM sessions ORDER BY updated_at DESC, rowid DESC").toArray();
  }

  // --- invites ---

  async createInvite(row: InviteRow): Promise<void> {
    this.sql.exec(
      "INSERT INTO invites (id, session_id, role, expires_at, revoked, created_at, uses) VALUES (?, ?, ?, ?, 0, ?, 0)",
      row.id, row.session_id, row.role, row.expires_at, row.created_at,
    );
  }

  async listInvites(sessionId: string | null): Promise<InviteRow[]> {
    const rows = sessionId === null
      ? this.sql.exec<InviteRow>("SELECT * FROM invites ORDER BY created_at DESC").toArray()
      : this.sql.exec<InviteRow>("SELECT * FROM invites WHERE session_id = ? ORDER BY created_at DESC", sessionId).toArray();
    return rows;
  }

  // Returns false when no such invite exists.
  async revokeInvite(id: string): Promise<boolean> {
    const existed = this.sql.exec("SELECT 1 AS one FROM invites WHERE id = ?", id).toArray().length > 0;
    if (existed) this.sql.exec("UPDATE invites SET revoked = 1 WHERE id = ?", id);
    return existed;
  }

  // Redeems a valid invite for a one-time ticket (60 s). Returns null when
  // the invite is unknown, revoked, expired, or its session is no longer
  // registered. The ticket carries the role and the session's viewer key.
  async issueTicket(inviteId: string, now: number): Promise<IssuedTicket | null> {
    this.sweepSessions(now);
    const invite = this.sql.exec<InviteRow>("SELECT * FROM invites WHERE id = ?", inviteId).toArray()[0];
    if (!invite || invite.revoked !== 0 || invite.expires_at <= now) return null;
    const session = this.sql
      .exec<SessionRow>("SELECT viewer_key FROM sessions WHERE id = ?", invite.session_id)
      .toArray()[0];
    if (!session) return null;
    this.sql.exec("DELETE FROM tickets WHERE expires_at <= ?", now);
    const ticket = randomHex(32);
    this.sql.exec(
      "INSERT INTO tickets (ticket, viewer_key, role, expires_at) VALUES (?, ?, ?, ?)",
      ticket, session.viewer_key, invite.role, now + TICKET_TTL_MS,
    );
    this.sql.exec("UPDATE invites SET uses = uses + 1 WHERE id = ?", inviteId);
    return { ticket, viewer_key: session.viewer_key, role: invite.role };
  }

  // Single use: the row is deleted whether or not the ticket still matches.
  // Returns the role only when the viewer key matches and the ticket is
  // unexpired.
  async redeemTicket(ticket: string, viewerKey: string, now: number): Promise<Role | null> {
    const row = this.sql
      .exec<{ viewer_key: string; role: Role; expires_at: number }>(
        "SELECT viewer_key, role, expires_at FROM tickets WHERE ticket = ?", ticket)
      .toArray()[0];
    this.sql.exec("DELETE FROM tickets WHERE ticket = ?", ticket);
    if (!row || row.viewer_key !== viewerKey || row.expires_at <= now) return null;
    return row.role;
  }

  // --- web push ---

  // The invite as it stands now: null when unknown, revoked or expired.
  async activeInvite(inviteId: string, now: number): Promise<InviteRow | null> {
    const invite = this.sql.exec<InviteRow>("SELECT * FROM invites WHERE id = ?", inviteId).toArray()[0];
    if (!invite || invite.revoked !== 0 || invite.expires_at <= now) return null;
    return invite;
  }

  async upsertPushSubscription(row: PushRow): Promise<void> {
    this.sql.exec(
      `INSERT OR REPLACE INTO push_subscriptions (id, endpoint, origin, p256dh, auth, role, session_id, created_at)
       VALUES (?, ?, ?, ?, ?, ?, ?, ?)`,
      row.id, row.endpoint, row.origin, row.p256dh, row.auth, row.role, row.session_id, row.created_at,
    );
  }

  async deletePushSubscription(id: string): Promise<boolean> {
    const existed = this.sql.exec("SELECT 1 AS one FROM push_subscriptions WHERE id = ?", id).toArray().length > 0;
    this.sql.exec("DELETE FROM push_subscriptions WHERE id = ?", id);
    return existed;
  }

  // Everything a Notify for `sessionId` needs: the session row (title and
  // viewer key, when registered) and every subscription that covers it.
  async pushTargets(sessionId: string, now: number): Promise<{ session: SessionRow | null; subs: PushRow[] }> {
    this.sweepSessions(now);
    const session = this.sql.exec<SessionRow>("SELECT * FROM sessions WHERE id = ?", sessionId).toArray()[0] ?? null;
    const subs = this.sql
      .exec<PushRow>("SELECT * FROM push_subscriptions WHERE session_id IS NULL OR session_id = ?", sessionId)
      .toArray();
    return { session, subs };
  }
  // --- provider-key vault (vault.ts) ---

  async putCredential(row: CredentialRow): Promise<void> {
    this.sql.exec(
      `INSERT OR REPLACE INTO credentials (provider_id, nonce, ciphertext, updated_at)
       VALUES (?, ?, ?, ?)`,
      row.provider_id, row.nonce, row.ciphertext, row.updated_at,
    );
  }

  // Returns false when no value was stored for the provider.
  async deleteCredential(providerId: string): Promise<boolean> {
    const existed = this.sql.exec("SELECT 1 AS one FROM credentials WHERE provider_id = ?", providerId).toArray().length > 0;
    this.sql.exec("DELETE FROM credentials WHERE provider_id = ?", providerId);
    return existed;
  }

  async listCredentials(): Promise<CredentialRow[]> {
    return this.sql.exec<CredentialRow>("SELECT provider_id, nonce, ciphertext, updated_at FROM credentials").toArray();
  }

  // --- GitHub accounts (github.ts) ---

  async listGithubAccounts(): Promise<Array<Omit<GithubAccountRow, "nonce" | "ciphertext">>> {
    return this.sql
      .exec<Omit<GithubAccountRow, "nonce" | "ciphertext">>("SELECT id, login, kind, created_at FROM github_accounts ORDER BY created_at, rowid")
      .toArray();
  }

  async countGithubAccounts(): Promise<number> {
    return this.sql.exec<{ n: number }>("SELECT COUNT(*) AS n FROM github_accounts").toArray()[0]?.n ?? 0;
  }

  async getGithubAccount(id: string): Promise<GithubAccountRow | null> {
    return this.sql.exec<GithubAccountRow>("SELECT * FROM github_accounts WHERE id = ?", id).toArray()[0] ?? null;
  }

  async putGithubAccount(row: GithubAccountRow): Promise<void> {
    this.sql.exec(
      "INSERT INTO github_accounts (id, login, kind, nonce, ciphertext, created_at) VALUES (?, ?, ?, ?, ?, ?)",
      row.id, row.login, row.kind, row.nonce, row.ciphertext, row.created_at,
    );
  }

  // Returns false when no such account exists.
  async deleteGithubAccount(id: string): Promise<boolean> {
    const existed = this.sql.exec("SELECT 1 AS one FROM github_accounts WHERE id = ?", id).toArray().length > 0;
    this.sql.exec("DELETE FROM github_accounts WHERE id = ?", id);
    return existed;
  }

  // --- cloud runs (runs.ts) ---

  async putRun(row: RunRow): Promise<void> {
    this.sql.exec(
      `INSERT INTO runs (id, repo, account_id, branch, state, reason, created_at, session_id, handoff_ref)
       VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)`,
      row.id, row.repo, row.account_id, row.branch, row.state, row.reason, row.created_at, row.session_id, row.handoff_ref,
    );
  }

  // Both reads carry viewer_key from the linked session (null when the run
  // has no session or the session is not registered).
  async getRun(id: string): Promise<RunView | null> {
    return this.sql
      .exec<RunView>(
        `SELECT r.*, s.viewer_key AS viewer_key FROM runs r
         LEFT JOIN sessions s ON s.id = r.session_id WHERE r.id = ?`, id)
      .toArray()[0] ?? null;
  }

  // Newest first, at most 100 rows.
  async listRuns(): Promise<RunView[]> {
    return this.sql
      .exec<RunView>(
        `SELECT r.*, s.viewer_key AS viewer_key FROM runs r
         LEFT JOIN sessions s ON s.id = r.session_id
         ORDER BY r.created_at DESC, r.rowid DESC LIMIT 100`)
      .toArray();
  }

  // Sets state and reason. Returns false when no such run exists.
  async setRunState(id: string, state: string, reason: string | null): Promise<boolean> {
    const existed = this.sql.exec("SELECT 1 AS one FROM runs WHERE id = ?", id).toArray().length > 0;
    if (existed) this.sql.exec("UPDATE runs SET state = ?, reason = ? WHERE id = ?", state, reason, id);
    return existed;
  }

  // The container stopped while the run was running (the platform's idle
  // timeout): sleeping. Conditional, so a stop or failure that landed first
  // keeps its state. Returns true when the run moved.
  async sleepRun(id: string): Promise<boolean> {
    const updated = this.sql.exec(
      "UPDATE runs SET state = 'sleeping' WHERE id = ? AND state = 'running'", id,
    );
    return updated.rowsWritten > 0;
  }

  // A host registered a session carrying this run id: the run is running and
  // linked to the session. Only a run that has not stopped or failed moves.
  async linkRunSession(id: string, sessionId: string): Promise<boolean> {
    const updated = this.sql.exec(
      "UPDATE runs SET state = 'running', reason = NULL, session_id = ? WHERE id = ? AND state IN ('starting', 'running', 'sleeping')",
      sessionId, id,
    );
    return updated.rowsWritten > 0;
  }
}
