// Reducer for Rust's versioned host projection. A reconnect without retained state starts with a snapshot.
/** @typedef {import('../../ox-host-protocol/generated/host-v1').Server} Server */
/** @typedef {import('../../ox-host-protocol/generated/host-v1').Scope} Scope */
export const scopeKey = (s) =>
  JSON.stringify([s.project, s.route.runtime, s.route.billing]);
export const emptyScope = (scope) => ({
  scope,
  connection: "connecting",
  capabilities: [],
  sessions: [],
  asks: [],
  transcripts: Object.create(null),
  detail: {},
});
const sessionKey = (scope, id) => scopeKey(scope) + " " + id;
const digestPattern = /^[a-zA-Z0-9_-]{43}$/;
export async function targetDigest(value) {
  const bytes = new TextEncoder().encode(value);
  const hash = new Uint8Array(await crypto.subtle.digest("SHA-256", bytes));
  return btoa(String.fromCharCode(...hash))
    .replaceAll("+", "-").replaceAll("/", "_").replaceAll("=", "");
}
// A Web Push reference includes the SHA-256 digest of the same four fields as
// push.rs. Older notifications carry only the native session id.
async function notificationMatch(ref, rows, complete) {
  const last = ref.lastIndexOf("~");
  const requestDigest = last >= 0 && ref.startsWith("ask=", last + 1)
    ? ref.slice(last + 5)
    : null;
  if (requestDigest != null && !digestPattern.test(requestDigest)) return "missing";
  const cut = requestDigest == null ? last : ref.lastIndexOf("~", last - 1);
  if (requestDigest != null && cut < 0) return "missing";
  const id = cut < 0 ? ref : ref.slice(0, cut);
  const candidates = rows.filter(({ session, request }) => session ? session.id === id : cut >= 0 && id === `ask:${request}`);
  if (cut < 0) {
    if (candidates.length > 1) return "ambiguous";
    if (!complete) return "incomplete";
    return candidates[0] ?? "missing";
  }
  const signature = ref.slice(cut + 1, requestDigest == null ? ref.length : last);
  if (!id || !digestPattern.test(signature)) return "missing";
  for (const row of candidates) {
    const { project, route } = row.scope;
    const alias = typeof route.billing === "string" ? null : route.billing.api;
    const hash = await targetDigest(JSON.stringify([project, route.runtime, alias, id]));
    if (hash === signature) return requestDigest == null ? row : { ...row, requestDigest };
  }
  return complete ? "missing" : "incomplete";
}

export class NotificationTarget {
  pending = null;

  request(ref) {
    this.pending = ref ? { ref, discovery: null } : null;
  }

  // A reconnect invalidates an in-flight digest but keeps the link for the
  // next catalog. User navigation or a newer tap replaces the request itself.
  reconnect() {
    if (this.pending) this.pending.discovery = null;
  }

  async resolve(rows, discovery, show) {
    const request = this.pending;
    if (!request || !discovery || request.discovery === discovery) return;
    request.discovery = discovery;
    let match;
    try {
      match = await notificationMatch(request.ref, rows, discovery.complete);
    } catch {
      match = "incomplete";
    }
    if (request !== this.pending || request.discovery !== discovery) return;
    this.pending = null;
    show(match);
  }
}

/**
 * The recorded parent of a session, from the immutable launch stamp when there
 * is one. The stamp carries the parent's full scope, so a worker in another
 * checkout or billing route still points at the conversation that started it.
 * @param {{scope: Scope, session: any}} entry
 * @returns {{project: string, scope: Scope|null, id: string, runtime?: string|null}|null}
 */
export function sessionParent(entry) {
  const origin = entry?.session?.detail?.origin;
  if (origin && origin.relation === "handoff") return null;
  const parent = origin?.parent;
  if (parent?.session)
    return {
      project: parent.project ?? entry.scope.project,
      scope: parent.scope ?? null,
      id: parent.session,
      runtime: parent.runtime,
    };
  if (entry?.session?.parent)
    return { project: entry.scope.project, scope: entry.scope, id: entry.session.parent };
  return null;
}
// External parents carry a native session id and a runtime, but no billing
// scope. Index all known rows so filtering the sidebar cannot hide a collision.
export function catalogParentResolver(entries) {
  const byProject = new Map();
  for (const entry of entries) {
    const { scope, session } = entry;
    const native = scope.route.runtime === "claude"
      ? /^claude:(?:api|subscription):([a-zA-Z0-9_-]+)$/.exec(session.id)
      : null;
    for (const id of native ? [session.id, native[1]] : [session.id]) {
      const key = JSON.stringify([scope.project, id]);
      const group = byProject.get(key) ?? [];
      group.push(entry);
      byProject.set(key, group);
    }
  }
  return (entry) => {
    const recorded = sessionParent(entry);
    if (!recorded) return null;
    const candidates = byProject.get(JSON.stringify([recorded.project, recorded.id])) ?? [];
    let { scope, id } = recorded;
    let match;
    if (scope) {
      match = candidates.find((row) =>
        row.session.id === id && scopeKey(row.scope) === scopeKey(scope));
    } else {
      const matches = candidates.filter((row) =>
        !recorded.runtime || row.scope.route.runtime === recorded.runtime);
      if (matches.length !== 1) return null;
      [match] = matches;
      scope = match.scope;
      id = match.session.id;
    }
    return {
      key: sessionKey(scope, id),
      scope,
      project: scope.project,
      id,
      title: match?.session.title ?? null,
    };
  };
}
/**
 * Group catalog entries into a worker tree. A parent may live in another
 * project or route; a filter that hides it keeps the child as a root with a
 * navigable recorded parent instead of dropping the relationship.
 * @param {Array<{scope: Scope, session: any}>} entries
 * @param {Array<{scope: Scope, session: any}>} catalog
 */
export function catalogTree(entries, catalog = entries) {
  const byKey = new Map(entries.map((entry) => [sessionKey(entry.scope, entry.session.id), entry]));
  const parentOf = catalogParentResolver(catalog);
  const links = new Map();
  const children = new Map();
  const roots = [];
  for (const entry of entries) {
    const own = sessionKey(entry.scope, entry.session.id);
    const target = parentOf(entry);
    if (target) links.set(own, target);
    if (target && target.key !== own && byKey.has(target.key)) {
      const group = children.get(target.key) ?? [];
      group.push(entry);
      children.set(target.key, group);
    } else {
      roots.push(entry);
    }
  }
  return { roots, children, links, byKey };
}
const recentDateFormat = new Intl.DateTimeFormat(undefined, {
  year: "numeric",
  month: "short",
  day: "numeric",
});
/**
 * Sort each session by its own activity, even when its parent is in another day.
 * @template {{scope: Scope, session: {id: string, updated_at: number}}} Entry
 * @param {Entry[]} entries
 * @param {Date} [now]
 */
export function recentDays(entries, now = new Date()) {
  const today = now.toDateString();
  const previous = new Date(now);
  previous.setDate(previous.getDate() - 1);
  const yesterday = previous.toDateString();
  const sorted = entries.map((entry) => {
    const seconds = entry.session.updated_at;
    const date = seconds > 0 ? new Date(seconds * 1000) : null;
    const timestamp = date?.getTime() || 0;
    return { entry, date, timestamp, key: sessionKey(entry.scope, entry.session.id) };
  });
  sorted.sort((a, b) => b.timestamp - a.timestamp || a.key.localeCompare(b.key));
  /** @type {Map<string, {label: string, entries: Entry[]}>} */
  const days = new Map();
  for (const { entry, date, timestamp } of sorted) {
    const day = timestamp && date ? date.toDateString() : "";
    let group = days.get(day);
    if (!group) {
      const label =
        day === today ? "Today"
          : day === yesterday ? "Yesterday"
          : day && date ? recentDateFormat.format(date)
          : "Date unknown";
      group = { label, entries: [] };
      days.set(day, group);
    }
    group.entries.push(entry);
  }
  return [...days.values()];
}

/** @param {{detail?: any}} session */
export function sessionResult(session) {
  const result = session?.detail?.last_result;
  return typeof result?.turn === "string" &&
    result.turn.length > 0 &&
    Number.isSafeInteger(result.completed_at) &&
    result.completed_at > 0 &&
    ["completed", "failed", "stopped"].includes(result.outcome)
    ? result
    : null;
}

const itemKey = (i) => JSON.stringify([i.turn, i.id]);
const safeMap = (entries) => Object.assign(Object.create(null), entries);
export class Projection {
  constructor(global = false) {
    this.global = global;
    this.scopes = new Map();
    this.cursor = null;
  }
  /** @param {Server} event */
  apply(event) {
    if (event.type === "snapshot") {
      this.scopes.clear();
      for (const scope of event.scopes) {
        scope.transcripts = this.global
          ? Object.create(null)
          : safeMap(scope.transcripts);
        this.scopes.set(scopeKey(scope.scope), scope);
      }
      this.cursor = event.cursor;
      return true;
    }
    if (event.type !== "delta") return false;
    if (this.cursor && event.cursor.epoch !== this.cursor.epoch)
      throw new Error("Host restarted");
    if (this.cursor && event.cursor.seq <= this.cursor.seq) return false;
    const key = scopeKey(event.scope);
    const s = this.scopes.get(key) ?? emptyScope(event.scope);
    this.scopes.set(key, s);
    let dirty = false;
    for (const c of event.changes) {
      switch (c.kind) {
        case "state":
          Object.assign(s, {
            connection: c.connection,
            capabilities: c.capabilities,
            detail: c.detail,
          });
          dirty = true;
          break;
        case "asks":
          s.asks = c.asks;
          dirty = true;
          break;
        case "sessions":
          s.sessions = c.sessions;
          dirty = true;
          break;
        case "session": {
          const i = s.sessions.findIndex((x) => x.id === c.session.id);
          if (i < 0) s.sessions.push(c.session);
          else s.sessions[i] = c.session;
          dirty = true;
          break;
        }
        case "retired":
          delete s.transcripts[c.thread];
          dirty = true;
          break;
        case "transcript":
          if (!this.global) {
            s.transcripts[c.thread] = c.timeline;
            dirty = true;
          }
          break;
        case "timeline_state":
          if (!this.global) {
            (s.transcripts[c.thread] ??= { items: [], detail: {} }).detail =
              c.detail;
            dirty = true;
          }
          break;
        case "items":
          if (!this.global) {
            const t = (s.transcripts[c.thread] ??= { items: [], detail: {} });
            const ix = new Map(t.items.map((v, i) => [itemKey(v), i]));
            for (const value of c.items) {
              const k = itemKey(value),
                i = ix.get(k);
              if (i === undefined) {
                ix.set(k, t.items.length);
                t.items.push(value);
              } else if (value.revision >= t.items[i].revision)
                t.items[i] = value;
            }
            dirty = true;
          }
          break;
        case "append":
          if (!this.global) {
            const row = s.transcripts[c.thread]?.items.findLast(
              (x) => x.turn === c.turn && x.id === c.id,
            );
            if (!row) throw new Error("Missing transcript item");
            if (c.revision > row.revision) {
              row.text += c.text;
              row.revision = c.revision;
              dirty = true;
            }
          }
          break;
      }
    }
    this.cursor = event.cursor;
    return dirty;
  }
}
