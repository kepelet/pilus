/**
 * The push relay: the one place that holds pilus's APNs key.
 *
 * pilus is peer to peer, so nothing is online to wake a sleeping phone. The
 * host daemon (`pilus-agent`) notices an agent start waiting and POSTs here
 * with the phone's device token; this Worker signs an APNs provider token and
 * forwards one alert. Keeping the `.p8` here means it never lands on a user's
 * machine, and the daemon needs no Apple credentials of its own.
 *
 * The relay stores nothing and logs no title or body: the text is the user's
 * session name, so it only passes through on its way to Apple.
 *
 * It takes no credential. Sending an alert needs the phone's device token,
 * which only the phone, Apple and the user's own host hold, and a key baked
 * into the open daemon would stop no one. Rate limits per caller and per token
 * bound the rest (see `docs/agents/push.md`).
 */

export interface Env {
  /** The `.p8` signing key, PEM text. */
  APNS_KEY: string;
  APNS_KEY_ID: string;
  APNS_TEAM_ID: string;
  /** The app's bundle id, which APNs calls the topic. */
  APNS_TOPIC: string;
  /** Requests per caller IP: caps what a stranger can cost us or the APNs key. */
  IP_LIMIT?: Limiter;
  /** Alerts per device token: caps how hard one phone can be spammed. */
  TOKEN_LIMIT?: Limiter;
}

/** The part of Cloudflare's rate-limit binding the relay uses. */
export interface Limiter {
  limit(options: { key: string }): Promise<{ success: boolean }>;
}

/** What the host sends. */
interface PushRequest {
  token: string;
  /** Which APNs environment minted the token; a hint, see {@link deliver}. */
  sandbox: boolean;
  title: string;
  body: string;
  /** Replaces an earlier alert with the same id, and names it for removal. */
  collapseId: string;
}

const PRODUCTION = "api.push.apple.com";
const SANDBOX = "api.sandbox.push.apple.com";

/** Apple wants a provider token refreshed at most every 20 minutes and never kept past 60. */
const TOKEN_TTL_MS = 40 * 60 * 1000;

/** An alert for an agent that is no longer waiting is noise; let APNs drop it. */
const EXPIRES_AFTER_SECONDS = 60 * 60;

const BODY_LIMIT = 4 * 1024;

type Fetcher = (input: string, init: RequestInit) => Promise<Response>;

/**
 * Build the request handler. The provider-token cache lives in the closure
 * rather than at module scope, so a test starts clean and a Worker isolate
 * still reuses the token across requests.
 */
export function createRelay(fetcher: Fetcher = fetch, now: () => number = Date.now) {
  let cached: { jwt: string; issuedAt: number } | null = null;

  async function providerToken(env: Env): Promise<string> {
    if (cached && now() - cached.issuedAt < TOKEN_TTL_MS) return cached.jwt;
    const issuedAt = now();
    const header = { alg: "ES256", kid: env.APNS_KEY_ID };
    const claims = { iss: env.APNS_TEAM_ID, iat: Math.floor(issuedAt / 1000) };
    const signing = `${base64url(JSON.stringify(header))}.${base64url(JSON.stringify(claims))}`;
    const key = await crypto.subtle.importKey(
      "pkcs8",
      pemToDer(env.APNS_KEY),
      { name: "ECDSA", namedCurve: "P-256" },
      false,
      ["sign"],
    );
    // WebCrypto signs ECDSA as raw r||s, which is the form JWT expects.
    const signature = await crypto.subtle.sign(
      { name: "ECDSA", hash: "SHA-256" },
      key,
      new TextEncoder().encode(signing),
    );
    const jwt = `${signing}.${base64url(new Uint8Array(signature))}`;
    cached = { jwt, issuedAt };
    return jwt;
  }

  async function send(env: Env, push: PushRequest, host: string): Promise<Response> {
    const jwt = await providerToken(env);
    return fetcher(`https://${host}/3/device/${push.token}`, {
      method: "POST",
      headers: {
        authorization: `bearer ${jwt}`,
        "apns-topic": env.APNS_TOPIC,
        "apns-push-type": "alert",
        "apns-priority": "10",
        "apns-collapse-id": push.collapseId,
        "apns-expiration": String(Math.floor(now() / 1000) + EXPIRES_AFTER_SECONDS),
      },
      body: JSON.stringify({
        aps: {
          alert: { title: push.title, body: push.body },
          sound: "default",
          "thread-id": push.collapseId,
        },
      }),
    });
  }

  /**
   * Deliver to the environment the phone named, and to the other one when
   * APNs says the token does not belong there. A token is only valid in the
   * environment that minted it, and a build's configuration (Debug vs a
   * TestFlight archive) is easy to get wrong, so the hint is allowed to be
   * wrong.
   */
  async function deliver(env: Env, push: PushRequest): Promise<Response> {
    const hosts = push.sandbox ? [SANDBOX, PRODUCTION] : [PRODUCTION, SANDBOX];
    let response = await send(env, push, hosts[0]);
    let reason = await reasonOf(response);
    if (reason === "ExpiredProviderToken") {
      cached = null;
      response = await send(env, push, hosts[0]);
      reason = await reasonOf(response);
    }
    if (reason === "BadDeviceToken") {
      response = await send(env, push, hosts[1]);
    }
    return response;
  }

  return async function handle(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);
    if (url.pathname !== "/v1/push") return json({ error: "not_found" }, 404);
    if (request.method !== "POST") return json({ error: "method_not_allowed" }, 405);

    // Counted before the body is read, so a flood of junk is refused cheaply.
    const caller = request.headers.get("cf-connecting-ip") ?? "unknown";
    if (!(await allowed(env.IP_LIMIT, `ip:${caller}`))) {
      return json({ error: "rate_limited" }, 429);
    }

    const raw = await request.text();
    if (raw.length > BODY_LIMIT) return json({ error: "too_large" }, 413);
    const push = parse(raw);
    if (!push) return json({ error: "bad_request" }, 400);
    if (!(await allowed(env.TOKEN_LIMIT, `token:${push.token}`))) {
      return json({ error: "rate_limited" }, 429);
    }

    let response: Response;
    try {
      response = await deliver(env, push);
    } catch {
      return json({ error: "unavailable" }, 502);
    }
    if (response.ok) return json({ ok: true }, 200);

    const reason = (await reasonOf(response)) ?? `status_${response.status}`;
    // The phone no longer has this token (app deleted, notifications revoked,
    // token rotated). Telling the host lets it stop sending.
    if (
      response.status === 410 ||
      reason === "BadDeviceToken" ||
      reason === "DeviceTokenNotForTopic" ||
      reason === "Unregistered"
    ) {
      return json({ error: "unregistered", reason }, 410);
    }
    return json({ error: "rejected", reason }, 502);
  };
}

function parse(raw: string): PushRequest | null {
  let value: unknown;
  try {
    value = JSON.parse(raw);
  } catch {
    return null;
  }
  if (typeof value !== "object" || value === null) return null;
  const { token, sandbox, title, body, collapse_id } = value as Record<string, unknown>;
  // Device tokens are hex today; Apple reserves the right to lengthen them.
  if (typeof token !== "string" || !/^[0-9a-f]{64,200}$/.test(token)) return null;
  if (typeof title !== "string" || title.length === 0 || title.length > 200) return null;
  if (typeof body !== "string" || body.length > 400) return null;
  // `apns-collapse-id` is capped at 64 bytes.
  if (typeof collapse_id !== "string" || collapse_id.length === 0 || collapse_id.length > 64) {
    return null;
  }
  if (sandbox !== undefined && typeof sandbox !== "boolean") return null;
  return { token, sandbox: sandbox === true, title, body, collapseId: collapse_id };
}

/**
 * Whether `limiter` lets `key` through. No binding (tests, local dev) or a
 * limiter that errors lets it through: a missed alert costs the user more
 * than a few extra requests cost us.
 */
async function allowed(limiter: Limiter | undefined, key: string): Promise<boolean> {
  if (!limiter) return true;
  try {
    return (await limiter.limit({ key })).success;
  } catch {
    return true;
  }
}

async function reasonOf(response: Response): Promise<string | null> {
  if (response.ok) return null;
  try {
    const { reason } = (await response.clone().json()) as { reason?: unknown };
    return typeof reason === "string" ? reason : null;
  } catch {
    return null;
  }
}

function json(value: unknown, status: number): Response {
  return new Response(JSON.stringify(value), {
    status,
    headers: { "content-type": "application/json" },
  });
}

function pemToDer(pem: string): ArrayBuffer {
  // A secret set through an env var may carry literal `\n` escapes.
  const base64 = pem.replace(/-----[^-]+-----/g, "").replace(/\\n|\s/g, "");
  const binary = atob(base64);
  const der = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) der[i] = binary.charCodeAt(i);
  return der.buffer;
}

function base64url(input: string | Uint8Array): string {
  const bytes = typeof input === "string" ? new TextEncoder().encode(input) : input;
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

const relay = createRelay();

export default {
  fetch: (request: Request, env: Env): Promise<Response> => relay(request, env),
};
