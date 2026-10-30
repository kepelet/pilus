import { describe, expect, test } from "bun:test";
import { createRelay, type Env } from "./worker.ts";

const TOKEN = "ab".repeat(32);

async function testEnv(): Promise<Env> {
  const pair = await crypto.subtle.generateKey({ name: "ECDSA", namedCurve: "P-256" }, true, [
    "sign",
  ]);
  const der = new Uint8Array(await crypto.subtle.exportKey("pkcs8", pair.privateKey));
  const base64 = btoa(String.fromCharCode(...der));
  return {
    APNS_KEY: `-----BEGIN PRIVATE KEY-----\n${base64}\n-----END PRIVATE KEY-----`,
    APNS_KEY_ID: "KEYID12345",
    APNS_TEAM_ID: "TEAMID1234",
    APNS_TOPIC: "com.kepelet.pilus",
  };
}

function push(overrides: Record<string, unknown> = {}): Request {
  return new Request("https://push.test/v1/push", {
    method: "POST",
    body: JSON.stringify({
      token: TOKEN,
      sandbox: false,
      title: "Claude is waiting your input",
      body: "fix the flaky test",
      collapse_id: "pilus-blocked-local-7",
      ...overrides,
    }),
  });
}

interface Call {
  url: string;
  headers: Record<string, string>;
  body: any;
}

/** A fetcher that answers from `replies` in order and records each call. */
function recorder(replies: Response[]) {
  const calls: Call[] = [];
  const fetcher = async (url: string, init: RequestInit) => {
    calls.push({
      url,
      headers: init.headers as Record<string, string>,
      body: JSON.parse(String(init.body)),
    });
    return replies[calls.length - 1] ?? new Response(null, { status: 200 });
  };
  return { calls, fetcher };
}

const rejection = (status: number, reason: string) =>
  new Response(JSON.stringify({ reason }), { status });

describe("push relay", () => {
  test("forwards one alert to production with the agreed headers", async () => {
    const env = await testEnv();
    const { calls, fetcher } = recorder([new Response(null, { status: 200 })]);
    const response = await createRelay(fetcher)(push(), env);

    expect(response.status).toBe(200);
    expect(calls).toHaveLength(1);
    expect(calls[0].url).toBe(`https://api.push.apple.com/3/device/${TOKEN}`);
    expect(calls[0].headers["apns-topic"]).toBe("com.kepelet.pilus");
    expect(calls[0].headers["apns-push-type"]).toBe("alert");
    expect(calls[0].headers["apns-collapse-id"]).toBe("pilus-blocked-local-7");
    expect(calls[0].body.aps.alert).toEqual({
      title: "Claude is waiting your input",
      body: "fix the flaky test",
    });
    // header.claims.signature, with the key id and team in the right places
    const jwt = calls[0].headers.authorization.replace("bearer ", "").split(".");
    expect(jwt).toHaveLength(3);
    expect(JSON.parse(atob(jwt[0].replace(/-/g, "+").replace(/_/g, "/")))).toEqual({
      alg: "ES256",
      kid: "KEYID12345",
    });
    expect(JSON.parse(atob(jwt[1].replace(/-/g, "+").replace(/_/g, "/"))).iss).toBe("TEAMID1234");
  });

  test("a sandbox token goes to the sandbox first", async () => {
    const env = await testEnv();
    const { calls, fetcher } = recorder([new Response(null, { status: 200 })]);
    await createRelay(fetcher)(push({ sandbox: true }), env);
    expect(calls[0].url).toStartWith("https://api.sandbox.push.apple.com/");
  });

  test("a wrong environment hint falls back to the other host", async () => {
    const env = await testEnv();
    const { calls, fetcher } = recorder([
      rejection(400, "BadDeviceToken"),
      new Response(null, { status: 200 }),
    ]);
    const response = await createRelay(fetcher)(push({ sandbox: true }), env);
    expect(response.status).toBe(200);
    expect(calls.map((call) => new URL(call.url).host)).toEqual([
      "api.sandbox.push.apple.com",
      "api.push.apple.com",
    ]);
  });

  test("a token neither environment knows is reported as unregistered", async () => {
    const env = await testEnv();
    const { fetcher } = recorder([rejection(400, "BadDeviceToken"), rejection(400, "BadDeviceToken")]);
    const response = await createRelay(fetcher)(push(), env);
    expect(response.status).toBe(410);
    expect((await response.json()) as any).toMatchObject({ error: "unregistered" });
  });

  test("APNs 410 is reported as unregistered", async () => {
    const env = await testEnv();
    const { fetcher } = recorder([rejection(410, "Unregistered")]);
    expect((await createRelay(fetcher)(push(), env)).status).toBe(410);
  });

  test("other APNs rejections are a bad gateway, not a reason to forget the phone", async () => {
    const env = await testEnv();
    const { fetcher } = recorder([rejection(429, "TooManyRequests")]);
    const response = await createRelay(fetcher)(push(), env);
    expect(response.status).toBe(502);
    expect((await response.json()) as any).toMatchObject({ reason: "TooManyRequests" });
  });

  test("the provider token is reused, then refreshed after it ages out", async () => {
    const env = await testEnv();
    let clock = 1_700_000_000_000;
    const { calls, fetcher } = recorder([]);
    const relay = createRelay(fetcher, () => clock);
    await relay(push(), env);
    await relay(push(), env);
    expect(calls[1].headers.authorization).toBe(calls[0].headers.authorization);
    clock += 41 * 60 * 1000;
    await relay(push(), env);
    expect(calls[2].headers.authorization).not.toBe(calls[0].headers.authorization);
  });

  test("an expired provider token is re-signed once", async () => {
    const env = await testEnv();
    let clock = 1_700_000_000_000;
    const { calls, fetcher } = recorder([
      rejection(403, "ExpiredProviderToken"),
      new Response(null, { status: 200 }),
    ]);
    const response = await createRelay(fetcher, () => (clock += 1000))(push(), env);
    expect(response.status).toBe(200);
    expect(calls).toHaveLength(2);
    expect(calls[1].headers.authorization).not.toBe(calls[0].headers.authorization);
  });

  test("refuses what is not a well-formed push", async () => {
    const env = await testEnv();
    const relay = createRelay(recorder([]).fetcher);
    for (const bad of [
      { token: "short" },
      { token: "AB".repeat(32) },
      { title: "" },
      { body: "x".repeat(401) },
      { collapse_id: "x".repeat(65) },
      { collapse_id: "" },
      { sandbox: "yes" },
    ]) {
      expect((await relay(push(bad), env)).status).toBe(400);
    }
    const notJson = new Request("https://push.test/v1/push", { method: "POST", body: "nope" });
    expect((await relay(notJson, env)).status).toBe(400);
  });

  test("only POST /v1/push exists", async () => {
    const env = await testEnv();
    const relay = createRelay(recorder([]).fetcher);
    expect((await relay(new Request("https://push.test/v1/push"), env)).status).toBe(405);
    expect((await relay(new Request("https://push.test/"), env)).status).toBe(404);
  });
});
